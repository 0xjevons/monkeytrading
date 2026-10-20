use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Fixed;

/// Milliseconds since the Unix epoch, UTC. Stamped by the sequencer; the
/// engine never reads a clock of its own.
pub type Millis = i64;

pub type MarketId = u32;
pub type OrderId = u64;

/// A trader account, keyed by the EVM address that owns it on the custody chain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub [u8; 20]);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid account address {0:?}")]
pub struct ParseAccountError(String);

impl FromStr for AccountId {
    type Err = ParseAccountError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseAccountError(s.to_owned());
        let hex = s.strip_prefix("0x").ok_or_else(err)?;
        if hex.len() != 40 {
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
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// +1 for buy, -1 for sell. Positions are signed: long > 0, short < 0.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }

    pub fn signed(self, qty: Fixed) -> Fixed {
        qty.mul_int(self.sign())
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
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
