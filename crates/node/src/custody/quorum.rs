//! Strict-majority custody reads. Each vote compares the same block or range; an empty log
//! response counts only after that provider also returns the range's last block. Finality tags
//! can advance between requests, so providers vote at the highest height a majority declares
//! final, with its hash independently read there. No answer is reused after losing quorum.

use std::collections::BTreeSet;
use std::sync::Mutex;

use alloy::primitives::Address;
use anyhow::{Context, bail};
use futures_util::future::join_all;
use tracing::{error, info, warn};

use crate::config::EvmCustodySection;
use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, VaultDeposit, VaultIdentity, VaultReader};
use crate::custody::rpc::VaultRpc;

/// Independent providers and their alert identities; the watcher owns the reader.
pub(crate) struct Quorum<R> {
    pub(super) providers: Vec<(String, R)>,
    required: usize,
    // VaultReader uses shared references. This lock only guards synchronous alert transitions,
    // never IO; all network futures finish before it is acquired.
    alerts: Mutex<Alerts>,
}

#[derive(Default)]
struct Alerts {
    providers: BTreeSet<(usize, &'static str, &'static str)>,
    lost: BTreeSet<&'static str>,
}

impl Quorum<VaultRpc> {
    /// Builds clients without accessing the network.
    /// Error cases:
    /// - fewer than three providers, or a client cannot be built
    pub(super) fn connect(cfg: &EvmCustodySection, vault: Address) -> anyhow::Result<Self> {
        let providers = cfg
            .providers
            .iter()
            .map(|p| Ok((p.name.clone(), VaultRpc::new(cfg, &p.rpc_url, vault)?)))
            .collect::<anyhow::Result<_>>()?;
        Self::new(providers)
    }
}

impl<R> Quorum<R> {
    /// Requires a strict majority of at least three configured providers.
    /// Error cases:
    /// - fewer than three providers
    pub(super) fn new(providers: Vec<(String, R)>) -> anyhow::Result<Self> {
        if providers.len() < 3 {
            bail!("custody quorum needs at least three providers");
        }
        Ok(Self { required: providers.len() / 2 + 1, providers, alerts: Mutex::default() })
    }

    /// Selects one identical answer with a strict majority and records alert transitions.
    /// Error cases:
    /// - insufficient identical successful responses; no value is returned or cached
    fn vote<T: Clone + PartialEq>(
        &self,
        operation: &'static str,
        answers: Vec<anyhow::Result<T>>,
    ) -> anyhow::Result<(T, Vec<usize>)> {
        let winner = majority(&answers, self.required);
        let mut alerts = self.alerts.lock().expect("quorum alert lock poisoned");
        for (index, answer) in answers.iter().enumerate() {
            let name = &self.providers[index].0;
            match answer {
                Err(cause) => {
                    if alerts.providers.insert((index, operation, "custody_rpc_provider_failed")) {
                        warn!(
                            alert = "custody_rpc_provider_failed",
                            provider = name,
                            operation,
                            "provider failed: {cause:#}"
                        );
                    }
                }
                Ok(value) => {
                    resolve(&mut alerts, index, name, operation, "custody_rpc_provider_failed");
                    if let Some(winner) = winner {
                        if value == winner {
                            resolve(&mut alerts, index, name, operation, "custody_rpc_provider_disagreed");
                        } else if alerts.providers.insert((index, operation, "custody_rpc_provider_disagreed")) {
                            warn!(
                                alert = "custody_rpc_provider_disagreed",
                                provider = name,
                                operation,
                                "provider disagrees with custody quorum"
                            );
                        }
                    }
                }
            }
        }
        let Some(value) = winner else {
            if alerts.lost.insert(operation) {
                error!(
                    alert = "custody_rpc_quorum_lost",
                    operation,
                    "custody requires {} identical votes; crediting paused, trading continues",
                    self.required
                );
            }
            bail!("custody RPC quorum lost for {operation}: need {} identical votes", self.required);
        };
        if alerts.lost.remove(operation) {
            info!(alert_resolved = "custody_rpc_quorum_lost", operation, "custody quorum restored");
        }
        let voters = answers
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.as_ref().is_ok_and(|other| other == value).then_some(i))
            .collect();
        Ok((value.clone(), voters))
    }
}

impl<R: VaultReader + Sync> VaultReader for Quorum<R> {
    /// The identity agreed by a strict majority.
    /// Error cases:
    /// - no quorum
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.vote("identity", join_all(self.providers.iter().map(|(_, r)| r.identity())).await).map(|v| v.0)
    }

    /// The highest common final height and its majority hash, with conservative head diagnostics.
    /// Error cases:
    /// - no quorum for finality or the block hash at the common height
    async fn finality(&self) -> anyhow::Result<Finality> {
        let mut answers = join_all(self.providers.iter().map(|(_, r)| r.finality())).await;
        let mut heights: Vec<_> = answers
            .iter()
            .filter_map(|a| a.as_ref().ok().filter(|f| f.tagged.is_none_or(|b| b.number <= f.head)))
            .filter_map(|f| f.tagged.map(|b| b.number))
            .collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        let mut candidates: Vec<_> = heights.into_iter().skip(self.required - 1).map(Some).collect();
        candidates.dedup();
        let absent = answers.iter().filter(|a| a.as_ref().is_ok_and(|f| f.tagged.is_none())).count();
        if absent >= self.required || candidates.is_empty() {
            candidates.push(None);
        }
        let mut votes = Vec::new();
        for common in candidates {
            votes = join_all(self.providers.iter().zip(&answers).map(|((_, reader), answer)| async move {
                let f = answer.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                if f.tagged.is_some_and(|b| b.number > f.head) {
                    bail!("finality tag is above the provider head");
                }
                match (common, f.tagged) {
                    (Some(number), Some(tagged)) if tagged.number >= number => {
                        let header = last_header(reader.headers(number, number).await?, number)?;
                        if tagged.number == number && tagged.hash != header.hash {
                            bail!("finality tag disagrees with the provider's header");
                        }
                        Ok(Some(BlockRef { number, hash: header.hash }))
                    }
                    (None, None) => Ok(None),
                    _ => bail!("provider has not reached the common final block"),
                }
            }))
            .await;
            if majority(&votes, self.required).is_some() {
                break;
            }
            // A lying high tag must not prevent the honest majority agreeing at a lower height.
            // Providers that contradicted their own tag stay excluded for this poll.
            for (answer, vote) in answers.iter_mut().zip(&votes) {
                let eligible = answer.as_ref().is_ok_and(|f| match (common, f.tagged) {
                    (Some(n), Some(b)) => b.number >= n,
                    _ => false,
                });
                if eligible && let Err(cause) = vote {
                    *answer = Err(anyhow::anyhow!("{cause:#}"));
                }
            }
        }
        let (tagged, voters) = self.vote("finality", votes)?;
        let heads = voters.iter().map(|&i| answers[i].as_ref().expect("a finality vote has an answer"));
        let head = heads.clone().map(|f| f.head).min().expect("a quorum is nonempty");
        let head_time = heads.map(|f| f.head_time).min().expect("a quorum is nonempty");
        Ok(Finality { head, head_time, tagged })
    }

    /// Identical ordered headers over the same height range.
    /// Error cases:
    /// - no quorum
    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        self.vote("headers", join_all(self.providers.iter().map(|(_, r)| r.headers(from, to))).await).map(|v| v.0)
    }

    /// Identical complete log sets and end-block hashes; a lagging absence cannot vote.
    /// Error cases:
    /// - no quorum, or a provider cannot return the range's last block
    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let answers = join_all(self.providers.iter().map(|(_, r)| async move {
            let header = last_header(r.headers(to, to).await?, to)?;
            Ok((header.hash, r.deposits(from, to).await?))
        }))
        .await;
        self.vote("deposits", answers).map(|v| v.0.1)
    }

    /// Identical counts at the same canonical block hash.
    /// Error cases:
    /// - no quorum
    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        self.vote("deposit_count", join_all(self.providers.iter().map(|(_, r)| r.deposit_count(at))).await).map(|v| v.0)
    }
}

/// The first value with enough identical successful answers; strict majorities cannot tie.
fn majority<T: PartialEq>(answers: &[anyhow::Result<T>], required: usize) -> Option<&T> {
    answers
        .iter()
        .filter_map(|a| a.as_ref().ok())
        .find(|value| answers.iter().filter(|a| a.as_ref().is_ok_and(|other| other == *value)).count() >= required)
}

/// Requires exactly the header requested, so a lagging absence carries a verified height.
/// Error cases:
/// - a missing, extra, or wrong-height header
fn last_header(headers: Vec<BlockLink>, number: u64) -> anyhow::Result<BlockLink> {
    let header = headers.first().copied().context("provider has no requested block")?;
    if headers.len() != 1 || header.number != number {
        bail!("provider returned the wrong header range");
    }
    Ok(header)
}

/// Ends one provider's alert once that operation has a healthy response.
fn resolve(alerts: &mut Alerts, index: usize, name: &str, operation: &'static str, alert: &'static str) {
    if alerts.providers.remove(&(index, operation, alert)) {
        info!(alert_resolved = alert, provider = name, operation, "provider recovered");
    }
}
