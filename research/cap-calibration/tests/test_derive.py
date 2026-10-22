"""The data derivation on small synthetic inputs: gap cleaning, holidays, hedges, and the check."""

from datetime import date, timedelta

import numpy as np
import pytest

from derive import markets, papertrade


def bars_at(hours, gaps):
    """Flat bars at `hours` after WEEK0; bar i opens `gaps[i]` above the previous close."""
    ts = np.array([int(markets.WEEK0.timestamp()) + 3600 * h for h in hours])
    close = np.ones(len(hours))
    open_ = close * np.exp(np.array(gaps))
    close = open_.copy()
    for i in range(1, len(hours)):
        open_[i] = close[i - 1] * np.exp(gaps[i])
        close[i] = open_[i]
    return ts, open_, open_.copy(), open_.copy(), close


def gaps_of(grid, hours):
    return grid[hours, 0] / markets.UNIT


def test_continuous_gaps_keep_breaks_weekends_and_holidays_only():
    thanksgiving = (date(2024, 11, 28) - markets.WEEK0.date()).days * 24
    hours = [0, 1, 3, 9, 100, 106, thanksgiving + 10, thanksgiving + 16, 200, 250]
    gaps = [0.0, 0.01, 0.02, 0.03, 0.0, 0.04, 0.0, 0.05, 0.0, 0.06]
    grid = markets.hourly_grid(*bars_at(hours, gaps), kind="continuous")
    got = gaps_of(grid, hours)
    assert got[1] == 0.0  # a one-hour step: contract alternation
    assert got[2] == pytest.approx(0.02, abs=1e-6)  # the daily break
    assert got[3] == 0.0  # a 6-hour weekday hole: a contract switch
    assert got[5] == 0.0
    assert got[7] == pytest.approx(0.05, abs=1e-6)  # a holiday halt
    assert got[9] == pytest.approx(0.06, abs=1e-6)  # 50 hours: a weekend


def test_share_gaps_are_kept_and_folded_indices_move_weekday_gaps_into_the_bar():
    hours, gaps = [0, 18, 85], [0.0, 0.02, 0.03]
    session = markets.hourly_grid(*bars_at(hours, gaps), kind="session")
    assert gaps_of(session, hours)[1] == pytest.approx(0.02, abs=1e-6)
    folded = markets.hourly_grid(*bars_at(hours, gaps), kind="folded")
    assert folded[18, 0] == 0 and folded[18, 3] / markets.UNIT == pytest.approx(0.02, abs=1e-6)
    assert folded[85, 0] / markets.UNIT == pytest.approx(0.03, abs=1e-6)  # a weekend stays a gap


def test_us_closure_dates():
    days = markets.us_closure_dates([2024, 2025, 2027])
    assert {date(2024, 11, 28), date(2024, 11, 29), date(2025, 4, 18), date(2027, 6, 18), date(2025, 1, 20)} <= days
    assert date(2024, 12, 20) not in days


def test_weekly_check_matches_a_series_against_itself():
    rng = np.random.default_rng(0)
    level = np.cumsum(rng.normal(0, 0.001, markets.HOURS))
    grid = np.zeros((markets.HOURS, 4), dtype=np.int32)
    grid[1:, 3] = np.round(np.diff(level) * markets.UNIT)
    closes = np.exp(np.cumsum(grid[:, 3] / markets.UNIT))
    fridays = [w * 168 + 119 for w in range(markets.WEEKS)]
    ts = np.array([markets.WEEK0.timestamp() + 3600 * h for h in fridays])
    corr, ratio = markets.weekly_check(grid, ts, closes[fridays])
    assert corr == pytest.approx(1.0) and ratio == pytest.approx(1.0)


def positions(rows):
    keys = ("user", "asset", "side", "open_order", "close_order", "notional", "open_block", "close_block")
    cols = {k: np.array([r[i] for r in rows], dtype=float if k in ("close_order", "notional", "close_block") else np.int64) for i, k in enumerate(keys)}
    cols["id"] = np.arange(len(rows))
    return cols


def test_hedges_flag_both_legs_within_and_across_accounts():
    pos = positions([
        (1, 0, 1, 100, 500, 10.0, 1, 5),  # account 1 long ...
        (1, 0, -1, 200, 400, 10.0, 2, 4),  # ... then short while the long is open: a hedge
        (2, 0, 1, 300, 900, 7.0, 3, 9),  # account 2 alone: directional
        (3, 0, 1, 1000, 2000, 5.0, 10, 20),  # accounts 3 and 4 pair up ...
        (4, 0, -1, 1100, 2100, 5.0, 11, 21),  # ... within two blocks at open and close
        (5, 0, -1, 1200, 2200, 6.0, 12, 22),  # a different notional: no pair
    ])
    assert papertrade.hedges(pos).tolist() == [1.0, 1.0, 0.0, 1.0, 1.0, 0.0]


def test_a_hedge_counts_only_the_notional_it_offsets_and_offsets_it_once():
    pos = positions([
        (1, 0, 1, 100, 900, 20.0, 1, 9),  # long 20 ...
        (1, 0, -1, 200, 900, 10.0, 2, 9),  # ... a short offsets half of it ...
        (1, 0, -1, 300, 900, 10.0, 3, 9),  # ... another short the other half ...
        (1, 0, -1, 400, 900, 10.0, 4, 9),  # ... and a third has nothing left to offset
        (2, 0, 1, 100, 900, 10.0, 1, 9),  # long 10 ...
        (2, 0, -1, 200, 300, 10.0, 2, 3),  # ... offset by a short that closes ...
        (2, 0, -1, 400, 900, 10.0, 4, 9),  # ... so a later short offsets the long again
        (2, 0, -1, 500, 900, 10.0, 5, 9),  # ... and the next one does not
    ])
    assert papertrade.hedges(pos).tolist() == [1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 0.0]
    partial = positions([(1, 0, 1, 100, 900, 20.0, 1, 9), (1, 0, -1, 200, 900, 10.0, 2, 9)])
    assert papertrade.hedges(partial).tolist() == [0.5, 1.0]


def test_margin_buys_notional_band_by_band():
    # BTC-USD: 10% to $1M, 20% above; shares: 10% to $500k, 20% to $2M, 50% above (market.rs,
    # staircase). At 10:1, $1M of margin buys $5.5M of BTC and $3.3M of a share, not $10M.
    btc = [{"up_to": "1000000", "bps": 1000}, {"bps": 2000}]
    share = [{"up_to": "500000", "bps": 1000}, {"up_to": "2000000", "bps": 2000}, {"bps": 5000}]
    margin, leverage = np.array([1e4, 1e6]), np.array([10.0, 1000.0])
    assert papertrade.mapped_notional(margin, leverage, btc, "capped").tolist() == pytest.approx([1e5, 5.5e6])
    assert papertrade.mapped_notional(margin, leverage, share, "capped").tolist() == pytest.approx([1e5, 3.3e6])
