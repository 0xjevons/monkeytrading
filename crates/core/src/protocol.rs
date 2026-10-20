//! Protocol versions. A version is a set of rules. A log moves to a newer version with
//! `AdminCommand::ActivateProtocol`, so each command applies under the rules active at its seq, and
//! a log that never activates one replays under `V2`, the rules every log starts under. Versions
//! are named after their reference logs in `crates/core/tests/reference/`; `V2`'s rules took effect
//! from genesis before versions could activate, so `v1.jsonl` and `v2.jsonl` both replay under it.
//!
//! Adding a version:
//! 1. Add a variant after the last one and point [`ProtocolVersion::LATEST`] at it; the node
//!    activates `LATEST` on its next boot, at the next seq of each log. Activation is one-way: a
//!    build that predates the version cannot read a log that activated it, so a rollback needs a
//!    fresh log.
//! 2. Gate each changed rule on `self.protocol_version >= ProtocolVersion::Vn`, and leave the
//!    earlier path as it is.
//! 3. Skip new engine state from serialization while it holds its default
//!    (`skip_serializing_if`), so roots before activation do not change. A request type that
//!    changes gets new typed structs under a new `eip712::DOMAIN_VERSION`, chosen by protocol
//!    version, and the earlier ones stay so earlier signatures still verify.
//! 4. Write and commit the version's reference log, as `crates/core/README.md` ("Adding a protocol
//!    version") describes. Every earlier reference log must still replay to its roots.

use serde::{Deserialize, Serialize};

/// A rule set, in activation order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolVersion {
    /// The rules every log starts under: the original rules (`v1.jsonl`), with venue quotes stamped
    /// too far ahead of the sequenced time refused (`v2.jsonl`). Anyone may act for any account.
    #[default]
    V2,
    /// Orders, cancels, and withdrawals are authorised as the log's `ClientAuth` records: by the
    /// account's EIP-712 signature, next nonce, and expiry, or unsigned in dev logs.
    V3,
    /// Schedules may carry dated closures and special sessions, and a house capital move of zero
    /// records its reference without moving cash.
    V4,
}

impl ProtocolVersion {
    /// The newest version this build implements.
    pub const LATEST: ProtocolVersion = ProtocolVersion::V4;

    /// Whether this is `V2`, which engine state leaves out of its encoding.
    pub fn is_v2(&self) -> bool {
        *self == ProtocolVersion::V2
    }
}
