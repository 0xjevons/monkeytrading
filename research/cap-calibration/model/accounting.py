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


def realised_pnl(side, exit_price, fx_ratio=1.0):
    """Realised PnL per unit of USD entry notional. A market quoted in another currency accrues PnL
    in that currency and converts it to USD at the FX index of the fill (#98), so its PnL carries
    `fx_ratio`, the FX index at exit over at entry."""
    return side * (exit_price - 1.0) * fx_ratio


def settle(pnl, balance, financing=0.0):
    """One account's exit: the profit fee, the bad debt written off against the house, and what is
    left to the client. Financing has already moved the balance at each rollover, so the account
    loses at most its balance after financing (`closeout.rs`, `settle_bad_debt`). Returns
    `(fee, bad_debt, proceeds)`; the house takes `-pnl + fee - bad_debt`."""
    fee = profit_fee(pnl)
    equity = balance + pnl - fee - financing
    bad = np.maximum(-equity, 0.0)
    return fee, bad, equity + bad


def daily_financing(side, notional, benchmark_bps, markup_bps, day_basis):
    """One rollover's financing, positive when the account pays: longs pay benchmark + markup,
    shorts pay markup - benchmark (`funds.rs`, `daily_financing`)."""
    annual_bps = np.where(np.asarray(side) > 0, benchmark_bps + markup_bps, markup_bps - benchmark_bps)
    return np.abs(notional) * annual_bps / 1e4 / day_basis


def exposure_cap(nav, cap_bps, floor):
    """A house exposure cap: the larger of a share of house NAV and a fixed floor (#15 section 2,
    PR #99)."""
    return np.maximum(np.asarray(nav) * cap_bps / 1e4, floor)


def order_room(exposure, cap, total_room):
    """The largest long and short order that fills against clients' net exposure `exposure` in a
    market: a fill that grows the absolute exposure must stay within the market cap and within
    `total_room`, the total cap less the gross exposure of the other markets; one that shrinks it
    always fills (`execution.rs`, `check_risk`). Returns `(long_room, short_room)`."""
    exposure = np.asarray(exposure, dtype=float)
    long_room = np.maximum(2 * np.maximum(-exposure, 0.0), np.minimum(cap - exposure, total_room - exposure))
    short_room = np.maximum(2 * np.maximum(exposure, 0.0), np.minimum(cap + exposure, total_room + exposure))
    return np.maximum(long_room, 0.0), np.maximum(short_room, 0.0)


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
