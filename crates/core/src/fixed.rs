//! Deterministic fixed-point arithmetic.
//!
//! Every price, quantity and cash amount in the engine is a [`Fixed`]: an `i128` scaled by 10^9.
//! Floats never touch engine state, so replaying the log on any machine gives identical results.
//! `mul`, `div`, `mul_bps`, and `div_int` truncate toward zero; `mul_div` and the `_rounded` methods
//! round the exact result once, in the direction the caller names. Release builds keep overflow
//! checks on, so an overflow halts the engine rather than corrupting state; external input is
//! bounded by [`Fixed::MAX_INPUT`] first, so validated input cannot overflow.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DECIMALS: u32 = 9;
const SCALE: i128 = 10i128.pow(DECIMALS);
const BPS: i128 = 10_000;

/// The internal representation of [`Fixed`]'s `Deserialize` input: a decimal string or an integer.
#[derive(Deserialize)]
#[serde(untagged)]
enum FixedRepr {
    Str(String),
    Int(i64),
}

/// Which way [`Fixed::mul_div`] and the `_rounded` methods round a result that falls between two
/// representable values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Toward negative infinity.
    Down,
    /// Toward positive infinity.
    Up,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fixed(i128);

#[derive(Debug, thiserror::Error)]
#[error("invalid fixed-point number {0:?}")]
pub struct ParseFixedError(String);

impl Add for Fixed {
    type Output = Fixed;

    /// Exact addition.
    fn add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 + rhs.0)
    }
}

impl Sub for Fixed {
    type Output = Fixed;

    /// Exact subtraction.
    fn sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 - rhs.0)
    }
}

impl Neg for Fixed {
    type Output = Fixed;

    /// Exact negation.
    fn neg(self) -> Fixed {
        Fixed(-self.0)
    }
}

impl AddAssign for Fixed {
    /// Exact in-place addition.
    fn add_assign(&mut self, rhs: Fixed) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Fixed {
    /// Exact in-place subtraction.
    fn sub_assign(&mut self, rhs: Fixed) {
        self.0 -= rhs.0;
    }
}

impl Sum for Fixed {
    /// Exact sum of an iterator of values.
    fn sum<I: Iterator<Item = Fixed>>(iter: I) -> Fixed {
        iter.fold(Fixed::ZERO, Add::add)
    }
}

impl fmt::Display for Fixed {
    /// Shortest exact decimal form: no trailing zeros, no exponent.
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
    /// Same as `Display`, so test failures read as numbers.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Fixed {
    type Err = ParseFixedError;

    /// Parses an optionally negative decimal with at most nine fractional digits.
    /// Error cases:
    /// - empty input, or no digits around the point
    /// - a character other than an ASCII digit, one leading `-`, or one `.`
    /// - more than nine fractional digits
    /// - a value outside the `i128` range at this scale
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

impl Serialize for Fixed {
    /// Serializes as a decimal string so no JSON client loses precision.
    /// Error cases:
    /// - the serializer rejects a string
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fixed {
    /// Accepts a decimal string or a whole integer.
    /// Error cases:
    /// - neither a string nor an integer
    /// - a string that does not parse as a `Fixed`
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match FixedRepr::deserialize(d)? {
            FixedRepr::Str(s) => s.parse().map_err(serde::de::Error::custom),
            FixedRepr::Int(n) => Ok(Fixed::from_int(n)),
        }
    }
}

impl Fixed {
    pub const ZERO: Fixed = Fixed(0);
    pub const ONE: Fixed = Fixed(SCALE);
    /// Largest magnitude accepted for any externally supplied number (qty, price, amount). Two
    /// such values multiply to at most 10^18, far inside the `i128` range at this scale, so engine
    /// arithmetic on validated input cannot overflow.
    pub const MAX_INPUT: Fixed = Fixed(1_000_000_000 * SCALE);

    /// A value from its raw scaled representation.
    pub const fn from_raw(raw: i128) -> Self {
        Fixed(raw)
    }

    /// The raw scaled representation.
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// A whole number.
    pub const fn from_int(n: i64) -> Self {
        Fixed(n as i128 * SCALE)
    }

    /// `bps` basis points as a fraction, e.g. `from_bps(50)` is 0.005.
    pub const fn from_bps(bps: i64) -> Self {
        Fixed(bps as i128 * SCALE / BPS)
    }

    // Named methods rather than `Mul`/`Div` impls, so truncation is visible at every call site.
    /// Product, truncated toward zero. The larger factor is split at the scale, so the full product
    /// never forms and only a result outside `Fixed`'s range overflows.
    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, rhs: Fixed) -> Fixed {
        let (big, small) = if self.0.abs() >= rhs.0.abs() { (self.0, rhs.0) } else { (rhs.0, self.0) };
        // `whole * small` is a whole number of units with the product's sign, so truncating the
        // part's share truncates the product.
        let (whole, part) = (big / SCALE, big % SCALE);
        Fixed(whole * small + part * small / SCALE)
    }

    /// Quotient, truncated toward zero. The dividend is split at the divisor, so only a result outside
    /// `Fixed`'s range, or a divisor above `i128::MAX / 10^9` in raw units, overflows. Panics on a
    /// zero divisor, which callers rule out first.
    #[allow(clippy::should_implement_trait)]
    pub fn div(self, rhs: Fixed) -> Fixed {
        assert!(rhs.0 != 0, "fixed-point division by zero");
        // `self` is `whole * rhs + part`, and the part's share has the quotient's sign.
        let (whole, part) = (self.0 / rhs.0, self.0 % rhs.0);
        Fixed(whole * SCALE + part * SCALE / rhs.0)
    }

    /// Product, rounded as `rounding` says. The larger factor is split at the scale, so the full
    /// product never forms and only a result outside `Fixed`'s range overflows.
    pub fn mul_rounded(self, rhs: Fixed, rounding: Rounding) -> Fixed {
        let (big, small) = if self.0.abs() >= rhs.0.abs() { (self.0, rhs.0) } else { (rhs.0, self.0) };
        // `big` is `whole * SCALE + part`; `whole * small` is a whole number of units, so only the
        // part's share needs rounding.
        let (whole, part) = (big / SCALE, big % SCALE);
        Fixed(whole * small + round_div(part * small, SCALE, rounding))
    }

    /// Quotient, rounded as `rounding` says. The dividend is split at the divisor, so only a result
    /// outside `Fixed`'s range, or a divisor above `i128::MAX / 10^9` in raw units, overflows. Panics
    /// on a zero divisor, which callers rule out first.
    pub fn div_rounded(self, rhs: Fixed, rounding: Rounding) -> Fixed {
        assert!(rhs.0 != 0, "fixed-point division by zero");
        // `self` is `whole * rhs + part`, so the quotient is `whole` units plus the part's share.
        let (whole, part) = (self.0 / rhs.0, self.0 % rhs.0);
        Fixed(whole * SCALE + round_div(part * SCALE, rhs.0, rounding))
    }

    /// `self * num / den` from the exact product, rounded once as `rounding` says. Panics on a zero
    /// `den`, which callers rule out first.
    pub fn mul_div(self, num: Fixed, den: Fixed, rounding: Rounding) -> Fixed {
        Fixed(round_div(self.0 * num.0, den.0, rounding))
    }

    /// `bps` basis points of this value, truncated toward zero.
    pub fn mul_bps(self, bps: i64) -> Fixed {
        Fixed(self.0 * bps as i128 / BPS)
    }

    /// `bps` basis points of this value, rounded as `rounding` says.
    pub fn mul_bps_rounded(self, bps: i64, rounding: Rounding) -> Fixed {
        Fixed(round_div(self.0 * bps as i128, BPS, rounding))
    }

    /// Product with a whole number.
    pub fn mul_int(self, n: i64) -> Fixed {
        Fixed(self.0 * n as i128)
    }

    /// Quotient by a whole number, truncated toward zero. Panics on zero, which callers rule out.
    pub fn div_int(self, n: i64) -> Fixed {
        assert!(n != 0, "fixed-point division by zero");
        Fixed(self.0 / n as i128)
    }

    /// Quotient by a whole number, rounded as `rounding` says. Panics on zero, which callers rule out.
    pub fn div_int_rounded(self, n: i64, rounding: Rounding) -> Fixed {
        Fixed(round_div(self.0, n as i128, rounding))
    }

    /// `self` relative to `base` in whole basis points, truncated, saturating at the `i64` range.
    /// Panics on a zero base, which callers rule out first.
    pub fn bps_of(self, base: Fixed) -> i64 {
        assert!(base.0 != 0, "bps_of zero base");
        (self.0 * BPS / base.0).clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Absolute value.
    pub fn abs(self) -> Fixed {
        Fixed(self.0.abs())
    }

    /// -1, 0 or 1.
    pub fn signum(self) -> i64 {
        self.0.signum() as i64
    }

    /// True for zero. Takes `&self` so serde's `skip_serializing_if` can call it.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// True above zero.
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// True below zero.
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// True for a value an external party may supply as a qty, price, or amount: above zero and
    /// at most [`Fixed::MAX_INPUT`].
    pub fn is_valid_input(self) -> bool {
        self.is_positive() && self <= Fixed::MAX_INPUT
    }

    /// Big-endian bytes of the raw representation, for signed digests.
    pub fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }
}

/// `n / d`, rounded as `rounding` says. Panics on a zero `d`, which callers rule out first.
fn round_div(n: i128, d: i128, rounding: Rounding) -> i128 {
    assert!(d != 0, "fixed-point division by zero");
    let (n, d) = if d < 0 { (-n, -d) } else { (n, d) };
    match rounding {
        Rounding::Down => n.div_euclid(d),
        Rounding::Up => -(-n).div_euclid(d),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    #[test]
    fn directed_rounding_brackets_the_exact_result_for_either_sign() {
        // 0.000000003 * 1.5 = 0.0000000045, and 1 / 3 = 0.333..., each of either sign.
        let cases = [
            (f("0.000000003").mul_rounded(f("1.5"), Rounding::Down), f("0.000000004")),
            (f("0.000000003").mul_rounded(f("1.5"), Rounding::Up), f("0.000000005")),
            (f("-0.000000003").mul_rounded(f("1.5"), Rounding::Down), f("-0.000000005")),
            (f("-0.000000003").mul_rounded(f("1.5"), Rounding::Up), f("-0.000000004")),
            (f("1").div_rounded(f("3"), Rounding::Down), f("0.333333333")),
            (f("1").div_rounded(f("3"), Rounding::Up), f("0.333333334")),
            (f("-1").div_rounded(f("3"), Rounding::Down), f("-0.333333334")),
            (f("-1").div_rounded(f("3"), Rounding::Up), f("-0.333333333")),
            (f("1").div_rounded(f("-3"), Rounding::Down), f("-0.333333334")),
            (f("1").div_rounded(f("-3"), Rounding::Up), f("-0.333333333")),
            // Exact results do not move.
            (f("2.5").mul_rounded(f("-4"), Rounding::Down), f("-10")),
            (f("2.5").mul_rounded(f("-4"), Rounding::Up), f("-10")),
            (f("-9").div_rounded(f("3"), Rounding::Down), f("-3")),
            (f("-9").div_rounded(f("3"), Rounding::Up), f("-3")),
        ];
        for (n, (got, want)) in cases.into_iter().enumerate() {
            assert_eq!(got, want, "case {n}");
        }
        // Products and quotients whose full intermediate exceeds `i128` but whose result fits.
        let big = Fixed::from_raw(18_000_000_000_000_000_000_000_000_000);
        assert_eq!(big.mul_rounded(f("10"), Rounding::Up), Fixed::from_raw(180_000_000_000_000_000_000_000_000_000));
        assert_eq!(f("10").mul_rounded(big, Rounding::Down), big.mul_int(10));
        let odd = big + Fixed::from_raw(1);
        assert_eq!(odd.mul_rounded(f("0.5"), Rounding::Down), Fixed::from_raw(big.raw() / 2));
        assert_eq!(odd.mul_rounded(f("0.5"), Rounding::Up), Fixed::from_raw(big.raw() / 2 + 1));
        let huge = big.mul_int(100) + Fixed::from_raw(1);
        assert_eq!(huge.div_rounded(f("0.1"), Rounding::Down), huge.mul_int(10));
        assert_eq!(huge.div_rounded(f("3"), Rounding::Down), Fixed::from_raw(huge.raw() / 3));
        assert_eq!(huge.div_rounded(f("3"), Rounding::Up), Fixed::from_raw(huge.raw() / 3 + 1));
        assert_eq!((-huge).div_rounded(f("3"), Rounding::Down), Fixed::from_raw(-huge.raw() / 3 - 1));
        // Over a grid of signs and magnitudes: down <= exact <= up, one unit apart unless exact.
        let values = [-7_000_000_001i128, -1_500_000_000, -3, -1, 1, 2, 999_999_999, 1_083_712_345, 151_370_000_000];
        for (a, b) in values.iter().flat_map(|a| values.iter().map(move |b| (*a, *b))) {
            for (num, den, down, up) in [
                (
                    a * b,
                    SCALE,
                    Fixed(a).mul_rounded(Fixed(b), Rounding::Down),
                    Fixed(a).mul_rounded(Fixed(b), Rounding::Up),
                ),
                (
                    a * SCALE,
                    b,
                    Fixed(a).div_rounded(Fixed(b), Rounding::Down),
                    Fixed(a).div_rounded(Fixed(b), Rounding::Up),
                ),
            ] {
                // Compare `down * den <= num <= up * den`, flipping the inequalities for a negative `den`.
                let (lo, hi) = (down.0 * den, up.0 * den);
                let bracketed = if den > 0 { lo <= num && num <= hi } else { lo >= num && num >= hi };
                assert!(bracketed, "{a} {b}: {down:?} {up:?}");
                assert_eq!(up.0 - down.0, i128::from(num % den != 0), "{a} {b}");
            }
        }
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in ["0", "1", "-1", "1.5", "-0.000000001", "123456.789"] {
            assert_eq!(f(s).to_string(), s);
        }
        assert_eq!(f(".5"), f("0.5"));
        // Ten fractional digits is more precision than the type holds.
        assert!("1.0000000001".parse::<Fixed>().is_err());
        assert!("abc".parse::<Fixed>().is_err());
        assert!("".parse::<Fixed>().is_err());
        assert!("1e5".parse::<Fixed>().is_err());
    }

    #[test]
    fn arithmetic_truncates_toward_zero() {
        assert_eq!(f("1.5").mul(f("2")), f("3"));
        assert_eq!(f("1").div(f("3")), f("0.333333333"));
        assert_eq!(f("-1").div(f("3")), f("-0.333333333"));
        assert_eq!(f("1000").mul_bps(25), f("2.5"));
        assert_eq!(Fixed::from_bps(1), f("0.0001"));
        assert_eq!((f("101") - f("100")).bps_of(f("100")), 100);
    }

    #[test]
    fn rounded_arithmetic_rounds_the_exact_result_once_in_the_direction_asked() {
        let (third, two_thirds) = (f("0.333333333"), f("0.666666667"));
        assert_eq!(f("1").mul_div(f("1"), f("3"), Rounding::Down), third);
        assert_eq!(f("1").mul_div(f("1"), f("3"), Rounding::Up), f("0.333333334"));
        assert_eq!(f("-1").mul_div(f("1"), f("3"), Rounding::Down), f("-0.333333334"));
        assert_eq!(f("-1").mul_div(f("1"), f("3"), Rounding::Up), -third);
        // A negative divisor flips the sign of the result, not the direction.
        assert_eq!(f("1").mul_div(f("1"), f("-3"), Rounding::Down), f("-0.333333334"));
        assert_eq!(f("2").mul_div(f("1"), f("3"), Rounding::Up), two_thirds);
        // Exact results are not moved.
        assert_eq!(f("1.5").mul_div(f("4"), f("3"), Rounding::Up), f("2"));
        assert_eq!(f("1.1").mul_rounded(f("0.000000001"), Rounding::Down), Fixed::from_raw(1));
        assert_eq!(f("1.1").mul_rounded(f("0.000000001"), Rounding::Up), Fixed::from_raw(2));
        assert_eq!(f("-1.1").mul_rounded(f("0.000000001"), Rounding::Down), Fixed::from_raw(-2));
        assert_eq!(f("-1.1").mul_rounded(f("0.000000001"), Rounding::Up), Fixed::from_raw(-1));
        assert_eq!(Fixed::from_raw(9).mul_bps_rounded(1_000, Rounding::Up), Fixed::from_raw(1));
        assert_eq!(Fixed::from_raw(-9).mul_bps_rounded(1_000, Rounding::Down), Fixed::from_raw(-1));
        assert_eq!(Fixed::from_raw(9).mul_bps_rounded(1_000, Rounding::Down), Fixed::ZERO);
        assert_eq!(f("1").div_int_rounded(3, Rounding::Up), f("0.333333334"));
        assert_eq!(f("-1").div_int_rounded(3, Rounding::Up), -third);
        assert_eq!(f("-1").div_int_rounded(3, Rounding::Down), f("-0.333333334"));
    }

    #[test]
    fn truncating_products_and_quotients_overflow_only_outside_the_range() {
        // 1.71e11 units at an index of 1e9: the scaled product, 1.71e38, is past `i128`, the result
        // is not. Each sign truncates toward zero, as the plain formula does where it fits.
        let qty = Fixed::from_int(171_000_000_000);
        let mid = Fixed::MAX_INPUT;
        assert_eq!(qty.mul(mid), Fixed::from_int(171_000_000_000).mul_int(1_000_000_000));
        assert_eq!((-qty).mul(mid), -qty.mul(mid));
        let odd = Fixed::from_raw(qty.raw() + 1);
        assert_eq!(odd.mul(f("0.5")), Fixed::from_raw(qty.raw() / 2));
        assert_eq!((-odd).mul(f("0.5")), Fixed::from_raw(-qty.raw() / 2));
        let huge = Fixed::from_raw(1_800_000_000_000_000_000_000_000_000_001);
        assert_eq!(huge.div(f("0.1")), huge.mul_int(10));
        assert_eq!(huge.div(f("3")), Fixed::from_raw(huge.raw() / 3));
        assert_eq!((-huge).div(f("3")), Fixed::from_raw(-huge.raw() / 3));
        // Where the plain formula fits, the result is the same, for every sign.
        let values = [-7_000_000_001i128, -1_500_000_000, -3, -1, 1, 2, 999_999_999, 1_083_712_345, 151_370_000_000];
        for (a, b) in values.iter().flat_map(|a| values.iter().map(move |b| (*a, *b))) {
            assert_eq!(Fixed(a).mul(Fixed(b)), Fixed(a * b / SCALE), "{a} * {b}");
            assert_eq!(Fixed(a).div(Fixed(b)), Fixed(a * SCALE / b), "{a} / {b}");
        }
    }

    #[test]
    fn bps_of_saturates_instead_of_wrapping() {
        // The smallest base against the largest input is ~10^22 bps, beyond `i64`.
        assert_eq!(Fixed::MAX_INPUT.bps_of(Fixed::from_raw(1)), i64::MAX);
        assert_eq!((-Fixed::MAX_INPUT).bps_of(Fixed::from_raw(1)), i64::MIN);
    }

    #[test]
    fn products_of_valid_inputs_do_not_overflow() {
        let max = Fixed::MAX_INPUT;
        assert!(max.is_valid_input());
        assert!(!(max + Fixed::from_raw(1)).is_valid_input());
        assert!(!Fixed::ZERO.is_valid_input());
        assert_eq!(max.mul(max).mul_bps(10_000), f("1000000000000000000"));
    }

    #[test]
    fn serde_as_string() {
        let json = serde_json::to_string(&f("1.25")).unwrap();
        assert_eq!(json, "\"1.25\"");
        assert_eq!(serde_json::from_str::<Fixed>("\"1.25\"").unwrap(), f("1.25"));
        assert_eq!(serde_json::from_str::<Fixed>("3").unwrap(), f("3"));
        assert!(serde_json::from_str::<Fixed>("1.5").is_err());
    }
}
