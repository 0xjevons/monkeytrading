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
    mean_sqrt_hold_h: float
    long_share: float


@dataclass
class Flow:
    classes: list[FlowClass]
    bucket_leverage: np.ndarray  # each leverage bin's notional over margin
    daily_margin: np.ndarray  # USD of margin opened per full papertrade day
    long_share_mean: float
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
            mean_sqrt_hold_h=float(r["mean_sqrt_hold_h"]),
            long_share=float(r["long_share"]),
        )
        for r in rows
    ]
    with (derived / "papertrade_daily.csv").open() as fh:
        daily = [float(r["opened_margin_usd"]) for r in csv.DictReader(fh) if r["full_day"] == "True"]
    summary = json.loads((derived / "papertrade_summary.json").read_text())
    return Flow(
        classes=classes,
        bucket_leverage=bucket_leverage,
        daily_margin=np.array(daily),
        long_share_mean=summary["long_share_mean"],
        long_share_ar1=summary["long_share_ar1"],
        long_share_resid_sd=summary["long_share_resid_sd"],
        long_share_slope=summary["long_share_slope_on_prev_hour_return"],
    )


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
