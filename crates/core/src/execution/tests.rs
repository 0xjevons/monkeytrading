use super::*;

fn f(n: i64) -> Fixed {
    Fixed::from_int(n)
}

#[test]
fn growth_covers_increase_flip_and_reduce() {
    assert_eq!(growth(f(0), f(3)), (f(0), f(3)));
    assert_eq!(growth(f(2), f(3)), (f(2), f(5)));
    // Long 2, sell 5: the 3 short is new risk.
    assert_eq!(growth(f(2), f(-5)), (f(0), f(3)));
    assert_eq!(growth(f(5), f(-2)), (f(0), f(0)));
    assert_eq!(growth(f(5), f(-5)), (f(0), f(0)));
}

#[test]
fn profit_fee_only_on_gains() {
    for version in [ProtocolVersion::V2, ProtocolVersion::V5] {
        assert_eq!(profit_fee(f(100), 1_000, version), f(10));
        assert_eq!(profit_fee(f(-100), 1_000, version), f(0));
        assert_eq!(profit_fee(f(0), 1_000, version), f(0));
    }
}

#[test]
fn a_fractional_profit_fee_rounds_up() {
    let realised = Fixed::from_raw(9);
    assert_eq!(profit_fee(realised, 1_000, ProtocolVersion::V5), Fixed::from_raw(1));
    assert_eq!(profit_fee(realised, 1_000, ProtocolVersion::V2), Fixed::ZERO);
    // The highest fee the engine accepts, 99.99%, takes the whole profit and no more.
    assert_eq!(profit_fee(realised, 9_999, ProtocolVersion::V5), realised);
}
