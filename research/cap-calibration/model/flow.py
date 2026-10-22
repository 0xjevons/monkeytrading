"""Trader flow: papertrade.xyz's measured behaviour, carried over to the launch markets.

Flow is carried over by margin (the collateral traders commit), not by notional, because the
launch markets allow far less leverage than papertrade's 1000x. Each papertrade flow class (kind,
hold class, leverage bin) brings its share of daily margin to every market in proportion to the
market mix, at a leverage mapped onto the market's own limit:
- `capped`: the papertrade leverage, capped at the market's maximum (most flow lands on the cap);
- `relative`: the same fraction of the maximum as of papertrade's 1000x.
"""

import csv
import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np

PAPERTRADE_MAX_LEVERAGE = 1000


@dataclass
class FlowClass:
    kind: str  # "directional" or "hedged"
    hold_h: int  # 0 = churn, opened and closed within the hour
    bucket: int  # papertrade leverage bin
    margin_share: float  # share of all papertrade margin
    mean_hold_h: float
    mean_sqrt_hold_h: float
    long_share: float


@dataclass
class Flow:
    classes: list[FlowClass]
    bucket_leverage: np.ndarray  # each leverage bin's notional over margin
    daily_margin: np.ndarray  # USD of margin opened per full papertrade day
    sizes: dict  # (kind, mapping, max leverage) -> (notional bounds, share of margin at or below each)
    long_share_ar1: float
    long_share_resid_sd: float
    long_share_slope: float  # on the previous hour's return


def load(derived: Path) -> Flow:
    with (derived / "papertrade_flow.csv").open() as fh:
        rows = list(csv.DictReader(fh))
    total = sum(float(r["margin_usd"]) for r in rows)
    bins = sorted({r["leverage_bin"] for r in rows}, key=lambda b: int(b.split("-")[0]))
    bucket_leverage = np.array(
        [
            sum(float(r["notional_usd"]) for r in rows if r["leverage_bin"] == b)
            / sum(float(r["margin_usd"]) for r in rows if r["leverage_bin"] == b)
            for b in bins
        ]
    )
    classes = [
        FlowClass(
            kind=r["kind"],
            hold_h=int(r["hold_class_h"]),
            bucket=bins.index(r["leverage_bin"]),
            margin_share=float(r["margin_usd"]) / total,
            mean_hold_h=float(r["mean_hold_h"]),
            mean_sqrt_hold_h=float(r["mean_sqrt_hold_h"]),
            long_share=float(r["long_share"]),
        )
        for r in rows
    ]
    with (derived / "papertrade_daily.csv").open() as fh:
        daily = [float(r["opened_margin_usd"]) for r in csv.DictReader(fh) if r["full_day"] == "True"]
    summary = json.loads((derived / "papertrade_summary.json").read_text())
    with (derived / "papertrade_sizes.csv").open() as fh:
        size_rows = list(csv.DictReader(fh))
    sizes = {}
    for r in size_rows:
        key = (r["kind"], r["mapping"], int(r["max_leverage"]))
        bounds, shares = sizes.setdefault(key, ([], []))
        bounds.append(float(r["notional_upto_usd"]))
        shares.append(float(r["margin_share"]))
    sizes = {k: (np.array(b), np.array(v)) for k, (b, v) in sizes.items()}
    return Flow(
        classes=classes,
        bucket_leverage=bucket_leverage,
        daily_margin=np.array(daily),
        sizes=sizes,
        long_share_ar1=summary["long_share_ar1"],
        long_share_resid_sd=summary["long_share_resid_sd"],
        long_share_slope=summary["long_share_slope_on_prev_hour_return"],
    )


def fit_share(room, sizes: tuple[np.ndarray, np.ndarray]):
    """Share of margin in orders whose notional is at most `room`: the engine refuses an order
    whose fill alone would pass the cap, whatever flows around it, so with no room nothing opens."""
    bounds, shares = sizes
    room = np.maximum(np.asarray(room, dtype=float), 0.0)
    fitted = np.interp(np.log(np.maximum(room, 1e-12)), np.log(bounds), shares, left=0.0, right=1.0)
    return np.where(room > 0, fitted, 0.0)


def mapped_leverage(papertrade_leverage: float, max_leverage: np.ndarray, mapping: str) -> np.ndarray:
    if mapping == "capped":
        return np.minimum(papertrade_leverage, max_leverage)
    if mapping == "relative":
        return np.maximum(1.0, papertrade_leverage / PAPERTRADE_MAX_LEVERAGE * max_leverage)
    raise ValueError(f"unknown leverage mapping {mapping}")


def daily_margin_path(flow: Flow, days: int, scale: float) -> np.ndarray:
    """Margin opened per simulated day: papertrade's full days in order, then the mean of its last
    seven, times `scale`."""
    observed = flow.daily_margin
    tail = observed[-7:].mean()
    path = np.concatenate([observed[:days], np.full(max(0, days - len(observed)), tail)])
    return path[:days] * scale


def load_mix(derived: Path, symbols: list[str]) -> np.ndarray:
    with (derived / "market_mix.csv").open() as fh:
        share = {r["symbol"]: float(r["share"]) for r in csv.DictReader(fh)}
    return np.array([share[s] for s in symbols])
