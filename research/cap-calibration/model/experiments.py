"""The calibration runs: per-market floor sweeps, joint runs over a common risk level, and the
sensitivities.

Insolvency measures, each a probability over the 90 days:
- `p_end`: house NAV is below zero at the end;
- `p_ever`: house NAV is below zero at some hour-end;
- `p_short`: at some hour-end the vault holds less than the free balances clients could withdraw
  on demand, which happens exactly when house cash plus the margin in open positions is negative;
- `p_queue`: under the modelled withdrawals, some withdrawal waits in the #49 queue.
The first three have a `_tail` form, read from the sample down to 20 paths and extrapolated below
that from the worst 5% of the underlying amount (NAV at the end, the lowest NAV, the lowest cover),
which the calibration uses. `p_dip_100k` and `p_dip_1m` give the chance the lowest NAV falls
below -$100k and -$1M.

Calibration, per measure: each market alone gives a curve of the measure against its floor. A
risk level `q` sets every market's floor to the largest tested floor at which the market alone
stays at or below `q`, no higher than the floor at which it stops refusing orders, and zero when
even a zero floor exceeds `q`; the total floor is the sum. The joint runs sweep `q`, and a target
takes the largest `q` whose joint measure meets it.
"""

import csv
import json
from dataclasses import replace
from pathlib import Path

import numpy as np

from model import simulate

TARGETS = (0.01, 0.001, 0.0001)
MEASURES = ("p_ever_tail", "p_short_tail")
MARKET_FLOORS = np.concatenate([[0.0], np.geomspace(1e4, 1e8, 17)])
LEVELS = (0.0, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2, 3e-2, 0.1, 1.0)  # 1.0: every market at its binding point
SCALES = np.array([0.25, 0.5, 1.0, 2.0, 4.0])
NO_TOTAL_LIMIT = 1e15


def tail_probability(values: np.ndarray, threshold: float = 0.0, tail: float = 0.05) -> float:
    """P(value < threshold): read from the sample when it holds at least 20 such paths, else
    extrapolated from an exponential fit to the worst `tail` of the sample. When neither works and
    no path is below the threshold, the 95% upper bound for zero events, 3 / paths."""
    n = len(values)
    hits = int((values < threshold).sum())
    if hits >= 20:
        return hits / n
    finite = values[np.isfinite(values)]
    if len(finite) >= 200:
        cut = np.quantile(finite, min(1.0, tail * n / len(finite)))  # the worst `tail` of all paths
        excess = cut - finite[finite < cut]
        if cut > threshold and len(excess) > 0 and excess.mean() > 0:
            return float(tail * np.exp(-(cut - threshold) / excess.mean()))
    return max(hits, 3) / n


def lane_stats(out: dict, lane: int) -> dict:
    nav_end = out["nav_end"][:, lane]
    return {
        "p_end": float((nav_end < 0).mean()),
        "p_end_tail": tail_probability(nav_end),
        "p_ever": float(out["ever_negative"][:, lane].mean()),
        "p_ever_tail": tail_probability(out["min_nav"][:, lane]),
        "p_queue": float(out["queued"][:, lane].mean()),
        "p_dip_100k": float((out["min_nav"][:, lane] < -1e5).mean()),
        "p_dip_1m": float((out["min_nav"][:, lane] < -1e6).mean()),
        "p_short": float((out["min_cover"][:, lane] < 0).mean()),
        "p_short_tail": tail_probability(out["min_cover"][:, lane]),
        "refused_share": float(out["refused"][:, lane].sum() / out["desired"][:, lane].sum()),
        "refused_share_directional": float(out["refused"][:, lane].sum() / max(out["desired_directional"][:, lane].sum(), 1e-9)),
        "nav_end_mean": float(nav_end.mean()),
        "nav_end_p01": float(np.quantile(nav_end, 0.01)),
        "nav_end_p50": float(np.quantile(nav_end, 0.5)),
        "min_nav_p01": float(np.quantile(out["min_nav"][:, lane], 0.01)),
        "min_nav_p50": float(np.quantile(out["min_nav"][:, lane], 0.5)),
        "fee_mean": float(out["fee"][:, lane].mean()),
        "financing_mean": float(out["financing"][:, lane].mean()),
        "bad_debt_mean": float(out["bad_debt"][:, lane].mean()),
        "bad_debt_p99": float(np.quantile(out["bad_debt"][:, lane], 0.99)),
        "mint_basis_mean": float(out["mint_basis"][:, lane].mean()),
        "max_queue_p99": float(np.quantile(out["max_queue"][:, lane], 0.99)),
        "max_gross_exposure_p50": float(np.quantile(out["max_gross_exposure"][:, lane], 0.5)),
        "paths": int(len(nav_end)),
    }


def binding_point(rows: list[dict], share: float = 0.01) -> float | None:
    """The smallest floor at which the directional refused share falls below `share`."""
    for row in sorted(rows, key=lambda r: r["floor"]):
        if row["refused_share_directional"] < share:
            return row["floor"]
    return None


def floor_at_level(rows: list[dict], key: str, level: float) -> float:
    """One market's floor at risk level `level`: the largest floor with every floor up to it at or
    below the level, no higher than the binding point; zero when a zero floor exceeds the level."""
    bind = binding_point(rows)
    best = 0.0
    for row in sorted(rows, key=lambda r: r["floor"]):
        if row[key] > level:
            break
        best = row["floor"]
    return min(best, bind) if bind is not None else best


def level_floors(rows: list[dict], symbols: list[str], key: str) -> np.ndarray:
    """(levels, markets) floors for `LEVELS`."""
    by_market = {s: [r for r in rows if r["symbol"] == s] for s in symbols}
    return np.array([[floor_at_level(by_market[s], key, level) for s in symbols] for level in LEVELS])


def per_market(scenario, markets, bars, flow, workers: int) -> list[dict]:
    """Each market alone, at its own share of the flow, over the floor grid, with no total cap."""
    floors = MARKET_FLOORS[:, None]
    total = np.full(len(MARKET_FLOORS), NO_TOTAL_LIMIT)
    tasks = [(scenario, markets.subset([i]), bars[:, [i], :], flow, floors, total) for i in range(len(markets.symbols))]
    rows = []
    for symbol, out in zip(markets.symbols, simulate.run_many(tasks, workers)):
        for lane, floor in enumerate(MARKET_FLOORS):
            rows.append({"symbol": symbol, "floor": float(floor), **lane_stats(out, lane)})
    return rows


def joint_rows(markets, out: dict, floors: np.ndarray, totals: np.ndarray, labels: list) -> list[dict]:
    rows = []
    for lane, label in enumerate(labels):
        refused = out["refused_by_market"][:, lane].sum(axis=0) / np.maximum(out["desired_by_market"][:, lane].sum(axis=0), 1e-9)
        pnl = out["pnl_by_market"][:, lane]
        rows.append(
            {
                "level": label,
                "total_floor": float(totals[lane]),
                **lane_stats(out, lane),
                "market_floors": dict(zip(markets.symbols, floors[lane].tolist())),
                "refused_by_market": dict(zip(markets.symbols, np.round(refused, 4).tolist())),
                "pnl_by_market_mean": dict(zip(markets.symbols, np.round(pnl.mean(axis=0), 0).tolist())),
                "pnl_by_market_p01": dict(zip(markets.symbols, np.round(np.quantile(pnl, 0.01, axis=0), 0).tolist())),
                "identity_gap": out["identity_gap"],
            }
        )
    return rows


def joint(scenario, markets, bars, flow, sweep: list[dict], workers: int) -> dict:
    """For each measure, all markets together at every risk level in `LEVELS`."""
    tasks, floor_sets = [], []
    for key in MEASURES:
        floors = level_floors(sweep, markets.symbols, key)
        floor_sets.append(floors)
        tasks.append((replace(scenario, name=f"joint {key}"), markets, bars, flow, floors, floors.sum(axis=1)))
    return {
        key: joint_rows(markets, out, floors, floors.sum(axis=1), list(LEVELS))
        for key, floors, out in zip(MEASURES, floor_sets, simulate.run_many(tasks, workers))
    }


def chosen_level(rows: list[dict], key: str, target: float) -> dict | None:
    """The joint row with the largest total floor whose measure meets `target`, every smaller
    level meeting it too; None when even zero floors miss it."""
    best = None
    for row in sorted(rows, key=lambda r: r["total_floor"]):
        if row[key] > target:
            break
        best = row
    return best


SENSITIVITIES = {
    "no farming": {"hedged_share": 0.0},
    "farming share x2": {"hedged_share": "x2"},
    "farming decays, 30-day half-life": {"farm_half_life_days": 30.0},
    "farm pairs held through closures": {"gap_pair_share": 0.1},
    "vol x1.5": {"vol_scale": 1.5},
    "vol x2": {"vol_scale": 2.0},
    "leverage relative to max": {"leverage_mapping": "relative"},
    "markets independent": {"independent_markets": True},
    "one client direction for all markets": {"common_direction": True},
    "flow x0.25": {"flow_scale": 0.25},
    "flow x4": {"flow_scale": 4.0},
    "exits before closures": {"hold_through_closures": False},
    "keep drift": {"demean": False},
}


def sensitivity(scenario, markets, bars, flow, floors: np.ndarray, measured_hedged: float, workers: int) -> list[dict]:
    """Every variant at `floors` scaled by `SCALES`, per-market and total together."""
    lanes = SCALES[:, None] * floors[None, :]
    names, tasks = [], []
    for name, change in {"baseline": {}, **SENSITIVITIES}.items():
        change = {k: (min(2 * measured_hedged, 0.95) if v == "x2" else v) for k, v in change.items()}
        names.append(name)
        tasks.append((replace(scenario, name=name, **change), markets, bars, flow, lanes, lanes.sum(axis=1)))
    rows = []
    for name, out in zip(names, simulate.run_many(tasks, workers)):
        for lane, scale in enumerate(SCALES):
            rows.append({"scenario": name, "floor_scale": float(scale), **lane_stats(out, lane)})
    return rows


def write_rows(path: Path, rows: list[dict]) -> None:
    flat = [{k: (json.dumps(v) if isinstance(v, dict) else v) for k, v in r.items()} for r in rows]
    with path.open("w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(flat[0]))
        writer.writeheader()
        writer.writerows(flat)

