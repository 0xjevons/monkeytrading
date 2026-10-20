//! The reference-log format: one JSON line per sequenced command, carrying the state root the
//! engine reached after applying it.

use engine_core::command::Sequenced;
use serde::{Deserialize, Serialize};

/// One line of a reference log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceEntry {
    #[serde(flatten)]
    pub sequenced: Sequenced,
    /// `0x`-prefixed keccak-256 state root after this command.
    pub root: String,
}

/// A state root as `0x`-prefixed lower-case hex.
pub fn root_hex(root: [u8; 32]) -> String {
    root.iter().fold(String::from("0x"), |hex, byte| hex + &format!("{byte:02x}"))
}
