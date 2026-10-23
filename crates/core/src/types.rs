//! Identifiers and small domain types shared across the engine.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Fixed;

/// Milliseconds since the Unix epoch, UTC. Stamped by the sequencer; the engine never reads a
/// clock of its own.
pub type Millis = i64;

/// The latest stamp the engine accepts: 9999-12-31T23:59:59.999Z. Anything later is refused
/// before it becomes engine time, so every time derived from a stamp — a session check, a
/// rollover — stays well inside chrono's range in any market's time zone, and the rollover walk
/// in `crate::params::Rollover::next_after` always has a representable day to step to.
pub(crate) const MAX_STAMP_MS: Millis = 253_402_300_799_999;

pub type MarketId = u32;
pub type OrderId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    Fx,
    Index,
    Commodity,
    Equity,
    Etf,
    Bond,
    Rates,
    Crypto,
}

/// A trader account, keyed by the EVM address that owns it on the custody chain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub [u8; 20]);

#[derive(Debug, thiserror::Error)]
#[error("invalid account address {0:?}")]
pub struct ParseAccountError(String);

impl fmt::Display for AccountId {
    /// Lower-case `0x`-prefixed hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for AccountId {
    /// Same as `Display`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for AccountId {
    type Err = ParseAccountError;

    /// Parses a `0x`-prefixed, 40-digit hex address in either case.
    /// Error cases:
    /// - no `0x` prefix
    /// - not exactly 40 hex digits after it
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseAccountError(s.to_owned());
        let hex = s.strip_prefix("0x").ok_or_else(err)?;
        if hex.len() != 40 || !hex.is_ascii() {
            return Err(err());
        }
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| err())?;
        }
        Ok(AccountId(out))
    }
}

impl Serialize for AccountId {
    /// Serializes as the `0x` hex string.
    /// Error cases:
    /// - the serializer rejects a string
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    /// Deserializes from the `0x` hex string.
    /// Error cases:
    /// - not a string
    /// - a string that is not a valid address
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

impl Side {
    /// +1 for buy, -1 for sell. Positions are signed: long > 0, short < 0.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }

    /// `qty` signed by this side.
    pub fn signed(self, qty: Fixed) -> Fixed {
        qty.mul_int(self.sign())
    }

    /// The other side.
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_round_trip_and_reject_malformed_input() {
        let id: AccountId = "0x00000000000000000000000000000000000000AA".parse().unwrap();
        assert_eq!(id.to_string(), "0x00000000000000000000000000000000000000aa");
        assert!("00000000000000000000000000000000000000aa".parse::<AccountId>().is_err());
        assert!("0x00aa".parse::<AccountId>().is_err());
        assert!("0x0000000000000000000000000000000000000zz0".parse::<AccountId>().is_err());
        // Multi-byte characters must not split a slice mid-character.
        assert!("0x00000000000000000000000000000000000000é".parse::<AccountId>().is_err());
    }
}
