//! Deterministic fixed-point arithmetic.
//!
//! Every price, quantity and cash amount in the engine is a [`Fixed`]: an
//! `i128` scaled by 10^9. Floats never touch engine state, so replaying the
//! log on any machine gives identical results. Multiplication and division
//! truncate toward zero. Release builds keep overflow checks on (see the
//! workspace profile), so an overflow halts the engine rather than
//! corrupting state.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DECIMALS: u32 = 9;
const SCALE: i128 = 10i128.pow(DECIMALS);
const BPS: i128 = 10_000;

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fixed(i128);

impl Fixed {
    pub const ZERO: Fixed = Fixed(0);
    pub const ONE: Fixed = Fixed(SCALE);

    pub const fn from_raw(raw: i128) -> Self {
        Fixed(raw)
    }

    pub const fn raw(self) -> i128 {
        self.0
    }

    pub const fn from_int(n: i64) -> Self {
        Fixed(n as i128 * SCALE)
    }

    /// `bps` basis points as a fraction, e.g. `from_bps(50)` is 0.005.
    pub const fn from_bps(bps: i64) -> Self {
        Fixed(bps as i128 * SCALE / BPS)
    }

    pub fn mul(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 * rhs.0 / SCALE)
    }

    pub fn div(self, rhs: Fixed) -> Fixed {
        assert!(rhs.0 != 0, "fixed-point division by zero");
        Fixed(self.0 * SCALE / rhs.0)
    }

    pub fn mul_bps(self, bps: i64) -> Fixed {
        Fixed(self.0 * bps as i128 / BPS)
    }

    pub fn mul_int(self, n: i64) -> Fixed {
        Fixed(self.0 * n as i128)
    }

    pub fn div_int(self, n: i64) -> Fixed {
        assert!(n != 0, "fixed-point division by zero");
        Fixed(self.0 / n as i128)
    }

    /// `self` relative to `base`, in whole basis points (truncated).
    pub fn bps_of(self, base: Fixed) -> i64 {
        assert!(base.0 != 0, "bps_of zero base");
        (self.0 * BPS / base.0) as i64
    }

    pub fn abs(self) -> Fixed {
        Fixed(self.0.abs())
    }

    pub fn signum(self) -> i64 {
        self.0.signum() as i64
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    pub fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }
}

impl Add for Fixed {
    type Output = Fixed;
    fn add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 + rhs.0)
    }
}

impl Sub for Fixed {
    type Output = Fixed;
    fn sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 - rhs.0)
    }
}

impl Neg for Fixed {
    type Output = Fixed;
    fn neg(self) -> Fixed {
        Fixed(-self.0)
    }
}

impl AddAssign for Fixed {
    fn add_assign(&mut self, rhs: Fixed) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Fixed {
    fn sub_assign(&mut self, rhs: Fixed) {
        self.0 -= rhs.0;
    }
}

impl Sum for Fixed {
    fn sum<I: Iterator<Item = Fixed>>(iter: I) -> Fixed {
        iter.fold(Fixed::ZERO, Add::add)
    }
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let int = abs / SCALE as u128;
        let frac = abs % SCALE as u128;
        if frac == 0 {
            return write!(f, "{sign}{int}");
        }
        let frac = format!("{frac:0width$}", width = DECIMALS as usize);
        write!(f, "{sign}{int}.{}", frac.trim_end_matches('0'))
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid fixed-point number {0:?}")]
pub struct ParseFixedError(String);

impl FromStr for Fixed {
    type Err = ParseFixedError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseFixedError(s.to_owned());
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        if int.is_empty() && frac.is_empty() {
            return Err(err());
        }
        if frac.len() > DECIMALS as usize {
            return Err(err());
        }
        let digits = |p: &str| p.is_empty() || p.bytes().all(|b| b.is_ascii_digit());
        if !digits(int) || !digits(frac) {
            return Err(err());
        }
        let int: i128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| err())? };
        let frac_raw: i128 = if frac.is_empty() {
            0
        } else {
            frac.parse::<i128>().map_err(|_| err())? * 10i128.pow(DECIMALS - frac.len() as u32)
        };
        let raw = int.checked_mul(SCALE).and_then(|v| v.checked_add(frac_raw)).ok_or_else(err)?;
        Ok(Fixed(if neg { -raw } else { raw }))
    }
}

/// Serialised as a decimal string so no JSON client loses precision.
impl Serialize for Fixed {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fixed {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Int(i64),
        }
        match Repr::deserialize(d)? {
            Repr::Str(s) => s.parse().map_err(serde::de::Error::custom),
            Repr::Int(n) => Ok(Fixed::from_int(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in ["0", "1", "-1", "1.5", "-0.000000001", "123456.789"] {
            assert_eq!(f(s).to_string(), s);
        }
        assert_eq!(f(".5"), f("0.5"));
        assert!("1.0000000001".parse::<Fixed>().is_err());
        assert!("abc".parse::<Fixed>().is_err());
        assert!("".parse::<Fixed>().is_err());
    }

    #[test]
    fn arithmetic() {
        assert_eq!(f("1.5").mul(f("2")), f("3"));
        assert_eq!(f("1").div(f("3")), f("0.333333333"));
        assert_eq!(f("1000").mul_bps(25), f("2.5"));
        assert_eq!(Fixed::from_bps(1), f("0.0001"));
        assert_eq!((f("101") - f("100")).bps_of(f("100")), 100);
    }

    #[test]
    fn serde_as_string() {
        let json = serde_json::to_string(&f("1.25")).unwrap();
        assert_eq!(json, "\"1.25\"");
        assert_eq!(serde_json::from_str::<Fixed>("\"1.25\"").unwrap(), f("1.25"));
        assert_eq!(serde_json::from_str::<Fixed>("3").unwrap(), f("3"));
    }
}
