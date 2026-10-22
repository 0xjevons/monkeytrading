"""The calibration runs: per-market floor sweeps, joint runs over a common risk level, and the
sensitivities.

Insolvency measures, each a probability over the 90 days:
- `p_end`: house NAV is below zero at the end;
- `p_ever`: house NAV is below zero at some hour-end;
- `p_short`: at some hour-end house cash is below zero, so the vault holds less than clients'
  balances. What clients may withdraw at once is never more than their balances, so this bounds
  from above the chance that the vault cannot pay a withdrawal on demand;
- `p_queue`: under the modelled withdrawals, some withdrawal waits in the #49 queue.
The first three have a `_tail` form, read from the sample down to 20 paths and extrapolated below
that from the worst 5% of the underlying amount (NAV at the end, the lowest NAV, the lowest house
cash),
which the calibration uses. `p_dip_100k` and `p_dip_1m` give the chance the lowest NAV falls
below -$100k and -$1M.

Calibration, per measure: each market alone gives a curve of the measure against its floor. A
risk level `q` sets every market's floor to the largest tested floor at which the market alone
stays at or below `q`, no higher than the floor at which it stops refusing orders, and zero when
even a zero floor exceeds `q`; the total floor is the sum. The joint runs sweep `q`, and a target
takes the largest `q` whose joint measure meets it. At the level chosen for 1%, a second joint run
sweeps the total floor below the sum, where the total cap starts to bind.
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
LEVELS = (0.0, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2, 3e-2, 0.1, 0.2, 0.3, 0.5, 1.0)  # 1.0: every market at its binding point
TOTAL_FRACTIONS = np.array([0.25, 0.5, 0.75, 1.0])  # total floor as a share of the market floors' sum
SCALES = np.array([0.1, 0.3, 1.0])  # sensitivity floor scales
NO_TOTAL_LIMIT = 1e15


def tail_probability(values: np.ndarray, threshold: float = 0.0, tail: float = 0.05) -> float:
    """P(value < threshold): read from the sample when it holds at least 20 such paths, else
    extrapolated from an exponential fit to the worst `tail` of the sample, never below the rate
    the sample shows. When the fit does not apply, the observed rate, or with no path below the
    threshold the 95% upper bound for zero events, 3 / paths."""
    n = len(values)
    hits = int((values < threshold).sum())
    if hits >= 20:
        return hits / n
    finite = values[np.isfinite(values)]
    if len(finite) == 0:  # no path ever opened anything: nothing can fall below zero
        return 0.0
    if len(finite) >= 200:
        cut = np.quantile(finite, min(1.0, tail * n / len(finite)))  # the worst `tail` of all paths
        excess = cut - finite[finite < cut]
        if cut > threshold and len(excess) > 0 and excess.mean() > 0:
            return max(hits / n, float(tail * np.exp(-(cut - threshold) / excess.mean())))
    return max(hits, 3) / n


def lane_stats(out: dict, lane: int) -> dict:
    nav_end = out["nav_end"][:, lane]
    opened = bool(np.isfinite(out["min_nav"][:, lane]).any())  # some path had cash or exposure
    tail = tail_probability if opened else (lambda values: 0.0)
    first_negative = out["first_negative_h"][:, lane][out["first_negative_h"][:, lane] >= 0]
    lowest_nav = np.where(np.isfinite(out["min_nav"][:, lane]), out["min_nav"][:, lane], 0.0)  # 0 where nothing opened
    return {
        "p_end": float((nav_end < 0).mean()),
        "p_end_tail": tail(nav_end),
        "p_ever": float(out["ever_negative"][:, lane].mean()),
        "p_ever_tail": tail(out["min_nav"][:, lane]),
        "p_queue": float(out["queued"][:, lane].mean()),
        "p_dip_100k": float((out["min_nav"][:, lane] < -1e5).mean()),
        "p_dip_1m": float((out["min_nav"][:, lane] < -1e6).mean()),
        "p_short": float((out["min_cash"][:, lane] < 0).mean()),
        "p_short_tail": tail(out["min_cash"][:, lane]),
        "refused_share": float((out["refused"][:, lane].sum() + out["refused_hedged"][:, lane].sum()) / max(out["desired"][:, lane].sum(), 1e-9)),
        "refused_share_directional": float(out["refused"][:, lane].sum() / max(out["desired_directional"][:, lane].sum(), 1e-9)),
        "refused_share_week1": float(out["refused_week1"][:, lane].sum() / max(out["desired_week1"][:, lane].sum(), 1e-9)),
        "first_negative_day_p50": float(np.median(first_negative) / 24) if len(first_negative) else -1.0,
        "nav_end_mean": float(nav_end.mean()),
        "nav_end_p01": float(np.quantile(nav_end, 0.01)),
        "nav_end_p50": float(np.quantile(nav_end, 0.5)),
        "min_nav_p01": float(np.quantile(lowest_nav, 0.01)),
        "min_nav_p50": float(np.quantile(lowest_nav, 0.5)),
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
    tasks = [(scenario, markets.subset([i]), bars, flow, floors, total) for i in range(len(markets.symbols))]
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


def total_sweep(scenario, markets, bars, flow, base: dict[str, dict], workers: int) -> dict:
    """For each measure, its base level's market floors with the total floor at `TOTAL_FRACTIONS`
    of their sum."""
    tasks, floor_sets = [], []
    for key in MEASURES:
        floors = np.array([base[key]["market_floors"][s] for s in markets.symbols])
        lanes = np.repeat(floors[None, :], len(TOTAL_FRACTIONS), axis=0)
        floor_sets.append((lanes, TOTAL_FRACTIONS * floors.sum()))
        tasks.append((replace(scenario, name=f"total {key}"), markets, bars, flow, lanes, TOTAL_FRACTIONS * floors.sum()))
    return {
        key: joint_rows(markets, out, lanes, totals, TOTAL_FRACTIONS.tolist())
        for key, (lanes, totals), out in zip(MEASURES, floor_sets, simulate.run_many(tasks, workers))
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

