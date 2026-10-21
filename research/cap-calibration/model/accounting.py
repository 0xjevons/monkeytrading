"""The engine rules the simulation applies, per unit of position notional.

Each function mirrors one rule in `crates/core` in floating point; the engine's `Fixed` rounding
(always in the house's favour, at 18 decimals) is far below the model's precision. Prices are
relative to the entry price, so a position's notional at entry is 1 and its price `x` is
`price / entry_price`. `side` is +1 for a long and -1 for a short.
"""

import numpy as np

PROFIT_FEE_BPS = 1000  # genesis.params.profit_fee_bps
CLOSEOUT_LEVEL_BPS = 5000  # genesis.params.closeout_level_bps


def profit_fee(realised, fee_bps: int = PROFIT_FEE_BPS):
    """Losers pay nothing; winners pay `fee_bps` of realised profit (`execution.rs`, `profit_fee`)."""
    return np.maximum(realised, 0.0) * fee_bps / 1e4


def margin_for(notional, tiers: list[dict]):
    """Margin for a position of `notional`: each band's rate on the slice inside it
    (`market.rs`, `staircase`)."""
    notional = np.abs(np.asarray(notional, dtype=float))
    total = np.zeros_like(notional)
    low = 0.0
    for tier in tiers:
        high = float(tier["up_to"]) if "up_to" in tier else np.inf
        total += np.clip(notional - low, 0.0, high - low) * tier["bps"] / 1e4
        low = high
    return total


def closeout_trigger(side, balance, margin_rate, level_bps: int = CLOSEOUT_LEVEL_BPS):
    """The relative price at which a one-position account's equity falls to the close-out level.

    Equity is `balance + side * (x - 1)` and the requirement is `margin_rate * x` at the mark
    (`engine.rs`, `equity_and_margin`); close-out starts when equity is below `level` of it
    (`closeout.rs`, `next_closeout`). A long triggers at or below the returned price, a short at or
    above it. For a market quoted in another currency, both sides of the comparison scale by the
    FX ratio `g` since entry, so pass `balance / g`.
    """
    level = level_bps / 1e4
    return (1.0 - side * balance) / (1.0 - side * level * margin_rate)


def closeout_fill(side, trigger, bar_open, bar_extreme):
    """Fill price of a close-out in a bar that reaches `trigger`: the bar's open when the market
    gapped through the trigger, else the trigger (continuous trading). `bar_extreme` is the low
    for a long and the high for a short. Returns NaN when the bar does not reach the trigger."""
    gapped = side * (bar_open - trigger) <= 0
    reached = side * (bar_extreme - trigger) <= 0
    return np.where(gapped, bar_open, np.where(reached, trigger, np.nan))


def settle(side, exit_price, balance, fx_ratio=1.0):
    """Client PnL credited at exit and bad debt written off against the house: the account loses
    at most its balance (`closeout.rs`, `settle_bad_debt`). A market quoted in another currency
    accrues PnL in that currency and converts it to USD at the FX index of the fill (#98), so its
    PnL per unit of USD entry notional carries `fx_ratio`, the FX index at exit over at entry."""
    pnl = side * (exit_price - 1.0) * fx_ratio
    return np.maximum(pnl, -balance), np.maximum(-pnl - balance, 0.0)


def daily_financing(side, notional, benchmark_bps, markup_bps, day_basis):
    """One rollover's financing, positive when the account pays: longs pay benchmark + markup,
    shorts pay markup - benchmark (`funds.rs`, `daily_financing`)."""
    annual_bps = np.where(np.asarray(side) > 0, benchmark_bps + markup_bps, markup_bps - benchmark_bps)
    return np.abs(notional) * annual_bps / 1e4 / day_basis


def exposure_cap(nav, cap_bps, floor):
    """A house exposure cap: the larger of a share of house NAV and a fixed floor (#15 section 2,
    PR #99)."""
    return np.maximum(np.asarray(nav) * cap_bps / 1e4, floor)


def accepted_share(exposure, cap, longs, shorts):
    """Share of new long and new short notional accepted when they would take the house's net
    client exposure past `cap`.

    The engine checks each order alone and refuses one whose fill would push the absolute net
    exposure past the cap (`execution.rs`, `check_risk`). Over one step the model nets the step's
    orders: the side that reduces exposure is accepted in full and the crowded side up to the
    cap. Returns `(long_share, short_share)`.
    """
    exposure, cap = np.asarray(exposure, dtype=float), np.asarray(cap, dtype=float)
    longs, shorts = np.asarray(longs, dtype=float), np.asarray(shorts, dtype=float)
    with np.errstate(divide="ignore", invalid="ignore"):
        long_room = np.where(longs > 0, (cap - exposure + shorts) / longs, 1.0)
        short_room = np.where(shorts > 0, (cap + exposure + longs) / shorts, 1.0)
    return np.clip(long_room, 0.0, 1.0), np.clip(short_room, 0.0, 1.0)
