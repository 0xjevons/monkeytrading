"""Derive the model's hourly price dataset and the per-market statistics from the raw pulls.

Outputs, under `data/derived/`:
- `hourly_bars.npz`: for every launch market and every UTC hour of the 102 whole weeks from
  `WEEK0`, four log returns in millionths: the bar's open against the previous bar's close
  (`gap`), and its high, low, and close against its own open. A closed hour is `CLOSED`.
- `market_stats.csv`: volatility, gap, and jump statistics per market, over the hourly window and
  over the long daily history.
- `market_mix.csv`: each market's share of trading volume, from its Hyperliquid listing's mean
  daily notional; a market with no listing takes its asset class's smallest listed volume.

Cleaning rules for hourly bars:
- A gap across a one-hour step is set to zero. A continuously traded market's next bar opens at the
  previous close; the gaps the public series show there are front- and next-month contracts
  alternating around a futures roll.
- For the nearly 24-hour markets (crypto, FX, futures), a gap after an unexplained weekday hole of
  3 to 24 hours is set to zero: those holes are where the public futures series switches contract.
  Gaps over the daily break (2 hours), weekends, and holidays (24 hours or more) are kept.
- For shares, the overnight gap is kept.
- DE40 and UK100 use cash indices, which close overnight, for venue futures that trade 20 or more
  hours a day. A weekday overnight gap is folded into the next bar as continuous movement. A
  weekend gap is taken to the first bar's close, because the FTSE series opens at the previous
  close and shows its opening move inside the first bar.
"""

import csv
import gzip
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import numpy as np

from catalogue import FRED_CHECKS, HYPERLIQUID, launch_markets

WEEK0 = datetime(2024, 10, 28, tzinfo=UTC)
WEEKS = 102
HOURS = WEEKS * 168
UNIT = 1e6
CLOSED = np.iinfo(np.int32).min
FOLDED = {"DE40", "UK100"}  # cash indices standing in for long-hours futures


def yahoo_bars(path: Path) -> tuple[np.ndarray, ...]:
    result = json.loads(gzip.decompress(path.read_bytes()))["chart"]["result"][0]
    quote = result["indicators"]["quote"][0]
    cols = [np.array(quote[k], dtype=float) for k in ("open", "high", "low", "close")]
    ts = np.array(result["timestamp"], dtype=np.int64)
    return (ts, *cols)


def binance_bars(path: Path) -> tuple[np.ndarray, ...]:
    rows = json.loads(gzip.decompress(path.read_bytes()))
    ts = np.array([r[0] // 1000 for r in rows], dtype=np.int64)
    cols = [np.array([float(r[i]) for r in rows]) for i in (1, 2, 3, 4)]
    return (ts, *cols)


def load_bars(raw: Path, source: str, ticker: str, interval: str) -> tuple[np.ndarray, ...]:
    if source == "binance":
        return binance_bars(raw / "binance" / f"{ticker}_{interval}.json.gz")
    return yahoo_bars(raw / "yahoo" / f"{ticker}_{interval}.json.gz")


def clean(ts, o, h, l, c):
    """Drops incomplete or non-positive bars and widens high and low to cover open and close."""
    ok = np.isfinite(o) & np.isfinite(h) & np.isfinite(l) & np.isfinite(c) & (np.minimum.reduce([o, h, l, c]) > 0)
    ts, o, h, l, c = ts[ok], o[ok], h[ok], l[ok], c[ok]
    h = np.maximum.reduce([h, o, c])
    l = np.minimum.reduce([l, o, c])
    return ts, o, h, l, c


def hourly_grid(ts, o, h, l, c, kind: str) -> np.ndarray:
    """(HOURS, 4) int32 grid of gap, high, low, close log returns, cleaned as the module says."""
    start = int(WEEK0.timestamp())
    hour = (ts - start) // 3600
    keep = (hour >= 0) & (hour < HOURS)
    hour, o, h, l, c = hour[keep], o[keep], h[keep], l[keep], c[keep]
    _, first = np.unique(hour, return_index=True)  # one bar per hour: the first
    hour, o, h, l, c = hour[first], o[first], h[first], l[first], c[first]

    gap = np.zeros(len(hour))
    gap[1:] = np.log(o[1:] / c[:-1])
    step = np.zeros(len(hour), dtype=np.int64)
    step[1:] = np.diff(hour)
    gap[step <= 1] = 0.0
    if kind == "continuous":
        gap[(step >= 3) & (step < 24)] = 0.0
    gap[0] = 0.0
    hi, lo, cl = np.log(h / o), np.log(l / o), np.log(c / o)
    if kind == "folded":
        weekday = (step > 1) & (step < 24)
        hi[weekday] = np.maximum(0.0, gap[weekday] + hi[weekday])
        lo[weekday] = np.minimum(0.0, gap[weekday] + lo[weekday])
        cl[weekday] = gap[weekday] + cl[weekday]
        gap[weekday] = 0.0
        weekend = step >= 24
        gap[weekend] = gap[weekend] + cl[weekend]
        hi[weekend] = lo[weekend] = cl[weekend] = 0.0

    grid = np.full((HOURS, 4), CLOSED, dtype=np.int32)
    for k, values in enumerate((gap, hi, lo, cl)):
        grid[hour, k] = np.round(values * UNIT).astype(np.int32)
    return grid


def path_returns(grid: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Per-hour close-to-close log return (zero when closed) and the gap series, as floats."""
    open_ = grid[:, 0] != CLOSED
    gap = np.where(open_, grid[:, 0], 0) / UNIT
    close = np.where(open_, grid[:, 3], 0) / UNIT
    return gap + close, np.where(open_, gap, np.nan)


def daily_closes(raw: Path, symbol: str, source: str, ticker: str) -> tuple[str, np.ndarray, np.ndarray]:
    """Long-history daily closes: FRED where an official series exists, else the public series,
    dividend-adjusted for shares."""
    if symbol in FRED_CHECKS:
        sid = FRED_CHECKS[symbol]
        rows = list(csv.reader(gzip.decompress((raw / "fred" / f"{sid}.csv.gz").read_bytes()).decode().splitlines()))
        dates, values = [], []
        for date, value in rows[1:]:
            if value not in ("", ".") and float(value) > 0:  # WTI settled below zero on 2020-04-20
                dates.append(datetime.fromisoformat(date).replace(tzinfo=UTC).timestamp())
                values.append(float(value))
        return f"FRED {sid}", np.array(dates), np.array(values)
    if source == "binance":
        ts, _, _, _, c = clean(*load_bars(raw, source, ticker, "1d"))
        return f"Binance {ticker} 1d", ts.astype(float), c
    result = json.loads(gzip.decompress((raw / "yahoo" / f"{ticker}_1d.json.gz").read_bytes()))["chart"]["result"][0]
    ts = np.array(result["timestamp"], dtype=float)
    close = np.array(result["indicators"]["quote"][0]["close"], dtype=float)
    adj = result["indicators"].get("adjclose", [{}])[0].get("adjclose")
    if adj is not None:
        close = np.array(adj, dtype=float)
    ok = np.isfinite(close) & (close > 0)
    return f"Yahoo {ticker} 1d", ts[ok], close[ok]


def margin_rate(market: dict) -> float:
    return market["margin_tiers"][0]["bps"] / 1e4


def stats_row(market: dict, grid: np.ndarray, daily: tuple) -> dict:
    hourly_ret, gaps = path_returns(grid)
    open_hours = grid[:, 0] != CLOSED
    days = hourly_ret.reshape(-1, 24).sum(axis=1)
    weeks = hourly_ret.reshape(-1, 168).sum(axis=1)
    bar_ret = (grid[open_hours, 3] / UNIT)
    bar_sd = bar_ret.std()
    reopen = gaps[np.isfinite(gaps) & (gaps != 0)]
    m = margin_rate(market)
    source, d_ts, d_close = daily
    d_ret = np.diff(np.log(d_close))
    span_years = (d_ts[-1] - d_ts[0]) / (365.25 * 86400)
    per_year = len(d_ret) / span_years
    return {
        "symbol": market["symbol"],
        "asset_class": market["asset_class"],
        "hourly_source": " ".join(market["series"][:2]),
        "margin_bps": market["margin_tiers"][0]["bps"],
        "open_hours_per_week": round(open_hours.sum() / WEEKS, 1),
        "vol_ann_window": round(days.std() * np.sqrt(365), 4),
        "daily_abs_p99_window": round(np.quantile(np.abs(days), 0.99), 4),
        "weekly_abs_p99_window": round(np.quantile(np.abs(weeks), 0.99), 4),
        "hourly_jumps_gt5sd_per_year": round(np.sum(np.abs(bar_ret) > 5 * bar_sd) / (WEEKS / 52.18), 1),
        "hourly_abs_max": round(np.abs(bar_ret).max(), 4),
        "reopen_gaps_per_year": round(len(reopen) / (WEEKS / 52.18), 1),
        "gap_abs_p99": round(np.quantile(np.abs(reopen), 0.99), 4) if len(reopen) else 0.0,
        "gap_abs_max": round(np.abs(reopen).max(), 4) if len(reopen) else 0.0,
        "gaps_past_half_margin_per_year": round(np.sum(np.abs(reopen) > m / 2) / (WEEKS / 52.18), 1),
        "gaps_past_margin_per_year": round(np.sum(np.abs(reopen) > m) / (WEEKS / 52.18), 1),
        "daily_source": source,
        "daily_from": datetime.fromtimestamp(d_ts[0], UTC).date().isoformat(),
        "vol_ann_long": round(d_ret.std() * np.sqrt(per_year), 4),
        "daily_abs_p99_long": round(np.quantile(np.abs(d_ret), 0.99), 4),
        "daily_abs_max_long": round(np.abs(d_ret).max(), 4),
        "daily_kurtosis_long": round(float(((d_ret - d_ret.mean()) ** 4).mean() / d_ret.var() ** 2 - 3), 1),
    }


def market_mix(raw: Path, markets: list[dict]) -> list[dict]:
    volume = {}
    for symbol, coin in HYPERLIQUID.items():
        candles = json.loads(gzip.decompress((raw / "hyperliquid" / f"{coin.replace(':', '_')}_1d.json.gz").read_bytes()))
        volume[symbol] = sum(float(c["v"]) * float(c["c"]) for c in candles) / max(len(candles), 1)
    floor_by_class = {}
    for m in markets:
        if m["symbol"] in volume:
            floor_by_class[m["asset_class"]] = min(floor_by_class.get(m["asset_class"], np.inf), volume[m["symbol"]])
    rows = []
    for m in markets:
        listed = m["symbol"] in volume
        usd = volume[m["symbol"]] if listed else floor_by_class[m["asset_class"]]
        rows.append({"symbol": m["symbol"], "hyperliquid": HYPERLIQUID.get(m["symbol"], ""), "usd_per_day": round(usd, 0), "listed": listed})
    total = sum(r["usd_per_day"] for r in rows)
    for r in rows:
        r["share"] = round(r["usd_per_day"] / total, 6)
    return rows


def derive(raw: Path, out: Path) -> None:
    markets = launch_markets()
    grids, rows = [], []
    for market in markets:
        source, ticker, _ = market["series"]
        kind = "folded" if market["symbol"] in FOLDED else "session" if market["asset_class"] == "equity" else "continuous"
        grid = hourly_grid(*clean(*load_bars(raw, source, ticker, "1h")), kind=kind)
        grids.append(grid)
        rows.append(stats_row(market, grid, daily_closes(raw, market["symbol"], source, ticker)))
    out.mkdir(parents=True, exist_ok=True)
    np.savez_compressed(
        out / "hourly_bars.npz",
        bars=np.stack(grids, axis=1),  # (HOURS, markets, 4)
        symbols=np.array([m["symbol"] for m in markets]),
        week0=np.array(WEEK0.isoformat()),
        unit=np.array(UNIT),
    )
    for name, table in (("market_stats.csv", rows), ("market_mix.csv", market_mix(raw, markets))):
        with (out / name).open("w", newline="") as fh:
            writer = csv.DictWriter(fh, fieldnames=list(table[0]))
            writer.writeheader()
            writer.writerows(table)
    end = WEEK0 + timedelta(weeks=WEEKS)
    print(f"hourly window {WEEK0.date()}..{end.date()}, {len(markets)} markets")
