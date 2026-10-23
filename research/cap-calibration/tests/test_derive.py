"""The data derivation on small synthetic inputs: gap cleaning, holidays, hedges, and the check."""

import csv
import gzip
import json
from collections import defaultdict
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


D0 = 1_699_920_000  # a UTC midnight (day 19675); the block clock below runs 100 s per block
# (user, side, margin, leverage, open block, close block, exit, adjusted pnl, fee). Blocks 0 and
# 3456 anchor to D0 and D0 + 4 days, so days 0 and 3 are partial — the first open lands after day
# 0's start and the last close before day 3's end — and their flow is nothing like the full days'.
POSITIONS = [
    ("0xa1", -1, 1e6, 1000, 72, 108, 49900, 1.6e6, 3.2e4),  # day 0: two huge 1000x shorts
    ("0xa2", -1, 1e6, 1000, 72, 108, 49900, 1.6e6, 3.2e4),
    ("0xa3", 1, 1e3, 10, 1692, 1728, 50100, 16, 0.32),  # day 1, full
    ("0xa4", 1, 1e3, 10, 1692, 1728, 50100, 16, 0.32),
    ("0xa5", 1, 1e3, 10, 1728, 1764, 50100, 16, 0.32),  # day 2, full
    ("0xa6", 1, 1e3, 10, 1728, 1764, 50100, 16, 0.32),
    ("0xa7", -1, 1e5, 500, 2628, 2664, 50050, -4e4, 800),  # day 3: one huge 500x short
]


def calibrate_logs(tmp_path):
    """Writes the synthetic papertrade logs and Binance klines, runs derive(), returns the out dir."""

    def words(*values):
        return "0x" + "".join(f"{int(round(v)) % (1 << 256):064x}" for v in values)

    def open_row(pid, user, side, margin, leverage, block, index):
        row = [block, 0, index, [papertrade.OPEN, hex(pid), user]]
        return row + [words(0, int(side > 0), 0, 50000 * 10, margin * 1e18, leverage)]

    def close_row(pid, user, exit_, adjusted, fee, block, index):
        row = [block, 0, index, [papertrade.CLOSE, hex(pid), user]]
        return row + [words(exit_ * 10, adjusted * 1e18, adjusted * 1e18, 0, fee * 1e18)]

    raw = tmp_path / "raw"
    by_block = defaultdict(list)
    for pid, (user, side, margin, lev, open_b, close_b, exit_, adjusted, fee) in enumerate(POSITIONS, start=1):
        by_block[open_b].append(open_row(pid, user, side, margin, lev, open_b, len(by_block[open_b])))
        by_block[close_b].append(close_row(pid, user, exit_, adjusted, fee, close_b, len(by_block[close_b])))
    (raw / "logs").mkdir(parents=True)
    for block, rows in sorted(by_block.items()):
        with gzip.open(raw / "logs" / f"{block}.jsonl.gz", "wt") as fh:
            for row in sorted(rows, key=lambda r: r[2]):
                fh.write(json.dumps(row) + "\n")
    (raw / "manifest.json").write_text(json.dumps({"from_block": 0, "last_block_pulled": 3456}))
    (raw / "block_times.csv").write_text(f"block,timestamp\n0,{D0}\n3456,{D0 + 4 * 86400}\n")
    klines = [[h * 3_600_000, 50000.0, 50000.0, 50000.0, 50000.0, 1.0] for h in range(D0 // 3600, D0 // 3600 + 97)]
    binance = tmp_path / "raw_markets" / "binance"
    binance.mkdir(parents=True)
    with gzip.open(binance / "BTCUSDT_1h_papertrade.json.gz", "wt") as fh:
        json.dump(klines, fh)
    out = tmp_path / "derived"
    papertrade.derive(raw, tmp_path / "raw_markets", out)
    return out


def read_csv(path):
    with path.open() as fh:
        return list(csv.DictReader(fh))


def test_flow_sizes_leverage_and_long_share_come_from_full_days_only(tmp_path):
    out = calibrate_logs(tmp_path)
    summary = json.loads((out / "papertrade_summary.json").read_text())

    # Only the four positions opened on days 1 and 2 reach the full-day sample the profiles use.
    assert summary["days"] == 3.0
    assert summary["full_days_positions"] == 4
    assert summary["full_days_hedged_share_of_notional"] == 0.0

    # The flow profile is just those four longs: one directional class, the 1-50 leverage bin,
    # $4k of margin; the partial days' 1000x and 500x shorts appear in no bin.
    (flow,) = read_csv(out / "papertrade_flow.csv")
    assert flow["kind"] == "directional" and int(flow["hold_class_h"]) == 1
    assert flow["leverage_bin"] == "1-50"
    assert float(flow["positions"]) == 4.0
    assert float(flow["notional_usd"]) == 40000.0
    assert float(flow["margin_usd"]) == 4000.0
    assert float(flow["long_share"]) == 1.0

    # All four full-day positions map to a single notional per schedule, so each margin share
    # jumps straight from 0 to 1; any partial-day leak (at 1e6 or 5e5 of mapped notional) lands
    # in between.
    shares = [float(r["margin_share"]) for r in read_csv(out / "papertrade_sizes.csv") if r["kind"] == "directional"]
    assert set(shares) == {0.0, 1.0}

    # The leverage profile holds only the 1-50 bin, with all $4k of the full-day margin.
    lev = [r for r in read_csv(out / "papertrade_leverage.csv") if r["kind"] == "directional"]
    assert {r["leverage_bin"] for r in lev} == {"1-50"}
    assert {float(r["margin_usd"]) for r in lev} == {4000.0}

    # The long-share fit sees the same days: every sampled open is long, over the two consecutive
    # hours of days 1 and 2; the partial days' shorts would pull the mean below 1.
    assert summary["long_share_mean"] == 1.0
    assert summary["long_share_resid_sd"] == 0.0
    assert summary["hours"] == 1

    # The daily CSV's full_day flag: the partial edge days are False, the covered middle days True.
    daily = read_csv(out / "papertrade_daily.csv")
    assert [r["full_day"] for r in daily] == ["False", "True", "True", "False"]
    assert [int(r["positions"]) for r in daily] == [2, 2, 2, 1]
