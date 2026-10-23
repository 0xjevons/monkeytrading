//! Linux CPU binding for the dedicated sequencer thread.

#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use tracing::info;
#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Builds the node runtime after reserving a CPU, so its workers inherit the remaining mask.
/// Error cases:
/// - CPU reservation, runtime construction, node execution, or restoring the calling mask fails
pub fn run_dedicated(cfg: crate::NodeConfig, telemetry: crate::Telemetry) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    let reservation = Reservation::new()?;
    #[cfg(target_os = "linux")]
    let cpu = Some(reservation.core);
    #[cfg(not(target_os = "linux"))]
    let cpu = None;
    let result = (|| {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        runtime.block_on(crate::node::run_with_cpu(cfg, telemetry, cpu))
    })();
    #[cfg(target_os = "linux")]
    reservation.restore()?;
    result
}

#[cfg(target_os = "linux")]
struct Reservation {
    original: nix::sched::CpuSet,
    core: usize,
}

#[cfg(target_os = "linux")]
impl Reservation {
    fn new() -> anyhow::Result<Self> {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;

        let original = sched_getaffinity(Pid::from_raw(0)).context("read node CPU mask")?;
        let cores: Vec<_> = (0..CpuSet::count()).filter(|cpu| original.is_set(*cpu).unwrap_or(false)).collect();
        anyhow::ensure!(cores.len() >= 2, "node requires at least two allowed CPUs to reserve the sequencer CPU");
        let core = *cores.last().expect("at least two CPUs");
        let mut workers = original;
        workers.unset(core).context("exclude sequencer CPU from workers")?;
        sched_setaffinity(Pid::from_raw(0), &workers).context("bind node workers away from sequencer CPU")?;
        Ok(Self { original, core })
    }

    fn restore(self) -> anyhow::Result<()> {
        nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &self.original)
            .context("restore calling thread CPU mask")
    }
}

/// Pins the sequencer thread to the last CPU allowed by the Linux deployment's affinity mask.
/// Error cases:
/// - reading or setting Linux CPU affinity fails, or the allowed mask is empty
#[cfg(target_os = "linux")]
pub(crate) fn pin(cpu: Option<usize>) -> anyhow::Result<()> {
    use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
    use nix::unistd::Pid;

    let allowed = sched_getaffinity(Pid::from_raw(0)).context("read sequencer CPU affinity")?;
    let core = cpu
        .or_else(|| (0..CpuSet::count()).rev().find(|cpu| allowed.is_set(*cpu).unwrap_or(false)))
        .context("no CPU allowed for sequencer")?;
    let mut selected = CpuSet::new();
    selected.set(core).context("select sequencer CPU")?;
    sched_setaffinity(Pid::from_raw(0), &selected).context("pin sequencer CPU")?;
    info!(core, "sequencer pinned to CPU");
    Ok(())
}

/// Development platforms run a dedicated thread without a Linux CPU binding guarantee.
#[cfg(not(target_os = "linux"))]
pub(crate) fn pin(_cpu: Option<usize>) -> anyhow::Result<()> {
    warn!("sequencer has a dedicated thread; CPU pinning requires Linux");
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn invalid_core_and_single_cpu_reservation_fail_without_corrupting_the_mask() {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;
        std::thread::spawn(|| {
            let original = sched_getaffinity(Pid::from_raw(0)).unwrap();
            assert!(super::pin(Some(CpuSet::count())).is_err());
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), original);
            super::pin(None).unwrap();
            let single = sched_getaffinity(Pid::from_raw(0)).unwrap();
            assert!(super::Reservation::new().is_err());
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), single);
            sched_setaffinity(Pid::from_raw(0), &original).unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn reserved_cpu_is_excluded_from_runtime_workers_and_restored_after_shutdown() {
        use nix::sched::{CpuSet, sched_getaffinity};
        use nix::unistd::Pid;
        std::thread::spawn(|| {
            let original = sched_getaffinity(Pid::from_raw(0)).unwrap();
            let count = (0..CpuSet::count()).filter(|cpu| original.is_set(*cpu).unwrap()).count();
            if count < 2 {
                assert!(super::Reservation::new().is_err());
                return;
            }
            let reservation = super::Reservation::new().unwrap();
            assert!(!sched_getaffinity(Pid::from_raw(0)).unwrap().is_set(reservation.core).unwrap());
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            runtime.block_on(async {
                let worker = tokio::spawn(async { sched_getaffinity(Pid::from_raw(0)).unwrap() }).await.unwrap();
                assert!(!worker.is_set(reservation.core).unwrap());
                let core = reservation.core;
                let sequencer = tokio::task::spawn_blocking(move || {
                    super::pin(Some(core)).unwrap();
                    sched_getaffinity(Pid::from_raw(0)).unwrap()
                })
                .await
                .unwrap();
                assert!(sequencer.is_set(core).unwrap());
                assert_eq!((0..CpuSet::count()).filter(|cpu| sequencer.is_set(*cpu).unwrap()).count(), 1);
            });
            drop(runtime);
            reservation.restore().unwrap();
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), original);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn pinning_restricts_only_its_thread_to_one_previously_allowed_cpu() {
        use nix::sched::{CpuSet, sched_getaffinity};
        use nix::unistd::Pid;
        let parent = sched_getaffinity(Pid::from_raw(0)).unwrap();
        std::thread::spawn(|| {
            let before = sched_getaffinity(Pid::from_raw(0)).unwrap();
            super::pin(None).unwrap();
            let after = sched_getaffinity(Pid::from_raw(0)).unwrap();
            let selected: Vec<_> = (0..CpuSet::count()).filter(|cpu| after.is_set(*cpu).unwrap()).collect();
            assert_eq!(selected.len(), 1);
            assert!(before.is_set(selected[0]).unwrap());
        })
        .join()
        .unwrap();
        assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), parent);
    }
}
