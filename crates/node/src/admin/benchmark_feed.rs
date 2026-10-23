//! The separate publisher worker: it alone holds the operator key and uses the signed admin API.

use anyhow::{Context, bail};
use engine_core::auth::Operator;
use engine_core::benchmark::BenchmarkState;
use engine_core::command::{AdminCommand, AdminRequest};
use engine_core::event::Event;
use engine_core::types::Millis;
use hyper::Method;
use k256::ecdsa::SigningKey;
use tracing::{info, warn};

use crate::admin::{EXPIRES_IN_MS, NodeApi, NonceView, call, sign, submit};
use crate::benchmarks;
use crate::config::NodeConfig;

/// Fetches and signs once, or repeats at the configured cadence until interrupted.
/// Error cases:
/// - missing benchmark config, client setup failure, or one-shot fetch/submission failure
pub(super) async fn run(
    cfg: &NodeConfig,
    node: &NodeApi,
    key: &SigningKey,
    operator: &Operator,
    clock: fn() -> Millis,
    watch: bool,
) -> anyhow::Result<AdminRequest> {
    let settings = cfg.benchmarks.as_ref().context("sync-benchmarks needs [benchmarks] in the config")?;
    let client = benchmarks::client()?;
    let mut ticks = tokio::time::interval(settings.interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = None;
    loop {
        tokio::select! {
            _ = ticks.tick() => {},
            stopped = tokio::signal::ctrl_c(), if watch => {
                stopped?;
                return last.context("benchmark worker stopped before its first snapshot");
            }
        }
        let attempt = async {
            let state: BenchmarkState = call(node, Method::GET, "/v1/benchmarks", None).await?;
            if state.rollover.is_none() {
                bail!("node genesis is still pending trusted time");
            }
            let batch = benchmarks::snapshot(settings, &client, &state, state.date_at(clock())).await?;
            let path = format!("/v1/accounts/{}/nonce", operator.address);
            let NonceView { next_nonce } = call(node, Method::GET, &path, None).await?;
            let req = sign(key, operator, AdminCommand::SetBenchmarkRates(batch), next_nonce, clock() + EXPIRES_IN_MS);
            let events = submit(node, &req).await?;
            if let Some(reason) = events.iter().find_map(|event| match event {
                Event::AdminRejected { reason } => Some(reason),
                _ => None,
            }) {
                bail!("benchmark snapshot rejected: {reason}");
            }
            info!(nonce = next_nonce, "signed benchmark snapshot sequenced");
            anyhow::Ok(req)
        }
        .await;
        match attempt {
            Ok(request) if !watch => return Ok(request),
            Ok(request) => last = Some(request),
            Err(err) if !watch => return Err(err),
            Err(err) => {
                warn!(alert = "benchmark_sync_failed", "benchmark worker will retry at its next interval: {err:#}")
            }
        }
    }
}
