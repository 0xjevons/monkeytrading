"""Monte Carlo of house NAV over the first months, hour by hour.

Each run carries `lanes`: sets of floors simulated on the same price paths and the same desired
flow, so floor levels compare without sampling noise between them.

Per hour and market:
1. Positions that reach their exit this hour settle: the planned exit after the class's hold, or
   a close-out once equity falls below 50% of margin, at the trigger in continuous trading or at
   the open when the market gaps through it. A loss beyond the balance is bad debt.
2. House NAV is house cash minus clients' unrealised PnL; each cap is the larger of its NAV share
   and its floor, per market and in total.
3. New directional flow is accepted up to the caps: the side that reduces the house's net
   exposure in full, the crowded side up to the cap. Refused notional is counted.
4. Hedged flow opens offsetting pairs in two accounts, which the caps never refuse; a pair ends
   when either leg is closed out (the other leg closes with it) or at its hold.
5. Flow held under an hour (churn) pays the profit fee on its expected gain over its hold.
6. Cash moves through the vault: exits return proceeds to idle balances, new margin is drawn
   from them first and the rest deposited, and a share of the proceeds left is withdrawn; idle
   balances leave at a daily rate, faster while NAV is below zero; a withdrawal the vault cannot
   pay waits in a first-in-first-out queue (#49).
"""

from dataclasses import dataclass

import numpy as np

from model import accounting
from model import prices as pricing
from model.flow import Flow, daily_margin_path, mapped_leverage

# Benchmark rates in bps a year: FRED on 2026-10-19 for SOFR (3.87%), SONIA (3.73%), and ESTR
# (2.44%); TONA and AONIA are assumptions near the Bank of Japan and Reserve Bank of Australia
# policy rates.
BENCHMARK_BPS = {"SOFR": 387, "SONIA": 373, "ESTR": 244, "TONA": 75, "AONIA": 360}
ROLLOVER_HOUR_UTC = 22  # 22:00 London, which is UTC through the winter launch window
GAIN_PER_SD = 1 / np.sqrt(2 * np.pi)  # E[max(Z, 0)] for a standard normal Z


@dataclass
class Scenario:
    name: str = "baseline"
    paths: int = 1000
    seed: int = 1
    days: int = 90
    flow_scale: float = 1.0  # times papertrade's margin flow
    hedged_share: float | None = None  # share of margin flow that is hedged; None = measured
    leverage_mapping: str = "capped"
    vol_scale: float = 1.0
    block_weeks: int = 4
    independent_markets: bool = False  # each market bootstraps its own weeks
    common_direction: bool = False  # one client long-share process for every market
    demean: bool = True  # remove each market's drift over the history
    hold_through_closures: bool = True  # a planned exit inside a closure waits for the open
    farm_half_life_days: float | None = None  # hedged flow decays with the MONKEY mint rate
    gap_pair_share: float = 0.0  # hedged margin of the past week held through each closure, at max leverage
    withdraw_on_exit: float = 0.5  # share of exit proceeds withdrawn at once
    idle_withdraw_per_day: float = 0.05
    run_withdraw_per_day: float = 0.5  # while NAV is below zero
    batch_paths: int = 250


@dataclass
class Markets:
    symbols: list[str]
    margin_rate: np.ndarray  # first margin tier
    cap_bps: np.ndarray
    total_cap_bps: int
    fin_long: np.ndarray  # financing per unit notional per rollover, paid by a long
    fin_short: np.ndarray
    mix: np.ndarray  # share of margin flow
    open_hours_per_week: np.ndarray

    @property
    def max_leverage(self) -> np.ndarray:
        return 1.0 / self.margin_rate

    def subset(self, idx: list[int]) -> "Markets":
        return Markets(
            symbols=[self.symbols[i] for i in idx],
            margin_rate=self.margin_rate[idx],
            cap_bps=self.cap_bps[idx],
            total_cap_bps=self.total_cap_bps,
            fin_long=self.fin_long[idx],
            fin_short=self.fin_short[idx],
            mix=self.mix[idx],
            open_hours_per_week=self.open_hours_per_week[idx],
        )


def load_markets(genesis: dict, symbols: list[str], mix: np.ndarray, bars: np.ndarray) -> Markets:
    by_symbol = {m["symbol"]: m for m in genesis["markets"]}
    specs = [by_symbol[s] for s in symbols]
    fin = [m["financing"] for m in specs]
    bench = np.array([BENCHMARK_BPS[f["benchmark"]] for f in fin])
    markup = np.array([f["markup_bps"] for f in fin])
    basis = np.array([f["day_basis"] for f in fin])
    open_hours = (bars[:, :, 0] != pricing.CLOSED).sum(axis=0) / (bars.shape[0] / pricing.WEEK)
    return Markets(
        symbols=list(symbols),
        margin_rate=np.array([m["margin_tiers"][0]["bps"] / 1e4 for m in specs]),
        cap_bps=np.array([m["house_cap_bps"] for m in specs]),
        total_cap_bps=genesis["params"]["house_total_cap_bps"],
        fin_long=accounting.daily_financing(1, 1.0, bench, markup, basis),
        fin_short=accounting.daily_financing(-1, 1.0, bench, markup, basis),
        mix=mix,
        open_hours_per_week=open_hours,
    )


@dataclass
class Prepared:
    """Everything one batch of paths needs that does not depend on the floors."""

    paths: pricing.Paths
    next_open: np.ndarray
    prev_open: np.ndarray
    rollovers: np.ndarray  # rollovers up to and including each hour
    margin_rate_hour: np.ndarray  # (paths, hours, markets) margin flow per hour
    leverage: np.ndarray  # (buckets, markets)
    trigger: np.ndarray  # (2, buckets, markets) close-out price, long then short
    first_cross: np.ndarray  # (2, buckets, paths, hours, markets) hours to the trigger; 0 = none
    pair_cash: np.ndarray  # (paths, hours, markets) house cash from pairs settling each hour
    pair_proceeds: np.ndarray
    pair_margin: np.ndarray  # margin pairs post each hour
    pair_totals: dict
    horizon: int  # hours from an entry to its latest possible settlement


def rollover_counts(hours: int) -> np.ndarray:
    return np.cumsum(np.arange(hours + 1) % 24 == ROLLOVER_HOUR_UTC)


def prev_open_hour(is_open: np.ndarray) -> np.ndarray:
    """(paths, hours + 1, markets): the last open hour at or before each hour; -1 when none."""
    n_paths, hours, n_markets = is_open.shape
    out = np.full((n_paths, hours + 1, n_markets), -1, dtype=np.int32)
    last = np.full((n_paths, n_markets), -1, dtype=np.int32)
    for t in range(hours):
        last = np.where(is_open[:, t], t, last)
        out[:, t] = last
    out[:, hours] = last
    return out


def first_crossings(paths: pricing.Paths, trigger: np.ndarray, horizon: int) -> np.ndarray:
    """Hours from each entry (at an hour's close) until the bar that reaches each trigger, within
    `horizon` hours; 0 when it is not reached."""
    log_close = np.log(paths.close).astype(np.float32)
    log_low = np.log(paths.low).astype(np.float32)
    log_high = np.log(paths.high).astype(np.float32)
    log_trigger = np.log(trigger).astype(np.float32)  # (2, buckets, markets)
    _, n_buckets, _ = trigger.shape
    n_paths, hours, n_markets = log_close.shape
    out = np.zeros((2, n_buckets, n_paths, hours, n_markets), dtype=np.int16)
    for k in range(1, min(horizon, hours - 1) + 1):
        down = log_low[:, k:] - log_close[:, :-k]
        up = log_high[:, k:] - log_close[:, :-k]
        for j in range(n_buckets):
            for side, move in ((0, down <= log_trigger[0, j]), (1, up >= log_trigger[1, j])):
                view = out[side, j, :, : hours - k]
                view[move & (view == 0)] = k
    return out


def prepare(scenario: Scenario, markets: Markets, bars: np.ndarray, flow: Flow, rng: np.random.Generator, n_paths: int) -> Prepared:
    hours = scenario.days * 24
    n_weeks = -(-hours // pricing.WEEK)
    history = bars.shape[0] // pricing.WEEK
    n_markets = len(markets.symbols)
    if scenario.independent_markets:
        weeks = np.stack([pricing.sample_weeks(rng, n_paths, n_weeks, history, scenario.block_weeks) for _ in range(n_markets)], axis=-1)
    else:
        weeks = pricing.sample_weeks(rng, n_paths, n_weeks, history, scenario.block_weeks)
    drift = pricing.drift_per_open_hour(bars) if scenario.demean else None
    paths = pricing.build(bars, weeks, hours, scenario.vol_scale, drift)
    next_open = pricing.next_open_hour(paths.is_open)
    prev_open = prev_open_hour(paths.is_open)

    day = np.arange(hours) // 24
    daily = daily_margin_path(flow, scenario.days, scenario.flow_scale)
    per_open_hour = daily[day][None, :, None] * markets.mix[None, None, :] * 7 / markets.open_hours_per_week[None, None, :]
    margin_rate_hour = np.where(paths.is_open, per_open_hour, 0.0)

    leverage = np.stack([mapped_leverage(b, markets.max_leverage, scenario.leverage_mapping) for b in flow.bucket_leverage])
    balance = 1.0 / leverage
    trigger = np.stack([accounting.closeout_trigger(side, balance, markets.margin_rate) for side in (1, -1)])
    reopen = next_open[:, 1:] - np.arange(hours)[None, :, None]
    longest_wait = int(reopen[next_open[:, 1:] < hours].max(initial=1))
    horizon = max(c.hold_h for c in flow.classes) + longest_wait
    first_cross = first_crossings(paths, trigger, horizon)

    prep = Prepared(paths, next_open, prev_open, rollover_counts(hours), margin_rate_hour, leverage, trigger, first_cross, None, None, None, {}, horizon)
    pair_flows(prep, scenario, markets, flow, hours)
    return prep


def hedged_scale(scenario: Scenario, flow: Flow) -> tuple[float, float]:
    """Multipliers on the measured hedged and directional margin shares for `hedged_share`."""
    measured = sum(c.margin_share for c in flow.classes if c.kind == "hedged")
    if scenario.hedged_share is None:
        return 1.0, 1.0
    return scenario.hedged_share / measured, (1 - scenario.hedged_share) / (1 - measured)


def farm_decay(scenario: Scenario, hours: int) -> np.ndarray:
    if scenario.farm_half_life_days is None:
        return np.ones(hours)
    return 0.5 ** (np.arange(hours) / 24 / scenario.farm_half_life_days)


def pair_flows(prep: Prepared, scenario: Scenario, markets: Markets, flow: Flow, hours: int) -> None:
    """Cash, proceeds, and margin of every hedged pair, by settlement hour. A pair's legs are
    equal and opposite, so it never changes the house's net exposure."""
    paths = prep.paths
    n_paths, _, n_markets = paths.close.shape
    cash = np.zeros((n_paths, hours + 1, n_markets))
    proceeds = np.zeros((n_paths, hours + 1, n_markets))
    margin = np.zeros((n_paths, hours, n_markets))
    totals = {k: np.zeros(n_paths) for k in ("fee", "financing", "bad_debt", "mint_basis", "notional")}
    h_scale, _ = hedged_scale(scenario, flow)
    decay = farm_decay(scenario, hours)[None, :, None]
    p = np.arange(n_paths)[:, None, None]
    m = np.arange(n_markets)[None, None, :]
    t = np.arange(hours)[None, :, None]
    fin_both = markets.fin_long + markets.fin_short  # per unit notional per rollover, both legs

    def settle_pairs(leg_notional, exit_hour, move, balance):
        """Books pairs with `leg_notional` per leg that settle at `exit_hour` with the price
        moved by `move` (relative, signed)."""
        gain = np.abs(move)
        loss = np.minimum(gain, balance)
        bad = gain - loss
        fee = accounting.profit_fee(gain)
        fin = (prep.rollovers[np.minimum(exit_hour, hours)] - prep.rollovers[np.broadcast_to(t, exit_hour.shape)]) * fin_both
        valid = exit_hour < hours
        house = np.where(valid, leg_notional * (fee + fin - bad), 0.0)
        client = np.where(valid, leg_notional * (2 * balance - loss + gain - fee - fin), 0.0)
        flat = ((np.broadcast_to(p, exit_hour.shape) * (hours + 1) + np.where(valid, exit_hour, hours)) * n_markets + np.broadcast_to(m, exit_hour.shape)).ravel()
        cash.reshape(-1)[:] += np.bincount(flat, house.ravel(), minlength=cash.size)
        proceeds.reshape(-1)[:] += np.bincount(flat, client.ravel(), minlength=proceeds.size)
        totals["fee"] += np.where(valid, leg_notional * fee, 0).sum(axis=(1, 2))
        totals["financing"] += np.where(valid, leg_notional * fin, 0).sum(axis=(1, 2))
        totals["bad_debt"] += np.where(valid, leg_notional * bad, 0).sum(axis=(1, 2))
        totals["mint_basis"] += np.where(valid, leg_notional * loss, 0).sum(axis=(1, 2))

    close = paths.close
    for cls in flow.classes:
        if cls.kind != "hedged" or cls.hold_h == 0:
            continue
        j = cls.bucket
        lev = prep.leverage[j][None, None, :]
        balance = 1.0 / lev
        leg = prep.margin_rate_hour * cls.margin_share * h_scale * decay / 2 * lev
        target = np.minimum(np.arange(hours) + cls.hold_h, hours)
        exit_planned = prep.next_open[:, target, :]
        if not scenario.hold_through_closures:
            last = prep.prev_open[:, target, :]
            exit_planned = np.where(last > t, last, exit_planned)
        cross = [prep.first_cross[s, j] for s in (0, 1)]
        cross_hour = [np.where(c > 0, t + c, np.iinfo(np.int32).max) for c in cross]
        first = np.minimum(cross_hour[0], cross_hour[1])
        closed_out = first <= exit_planned
        exit_hour = np.where(closed_out, first, exit_planned)
        loser_long = cross_hour[0] <= cross_hour[1]
        trig = np.where(loser_long, prep.trigger[0, j][None, None, :], prep.trigger[1, j][None, None, :])
        side = np.where(loser_long, 1.0, -1.0)
        bar_open = paths.open[p, np.minimum(exit_hour, hours - 1), m] / close
        fill = np.where(side * (bar_open - trig) <= 0, bar_open, trig)
        at_exit = paths.close[p, np.minimum(exit_hour, hours - 1), m] / close
        move = np.where(closed_out, fill, at_exit) - 1.0
        settle_pairs(leg, exit_hour, move, balance)
        margin += 2 * leg * balance
        totals["notional"] += 2 * leg.sum(axis=(1, 2))

    if scenario.gap_pair_share > 0:
        balance = markets.margin_rate[None, :]
        weekly = prep.margin_rate_hour[:, :, :].sum(axis=1) / max(hours / pricing.WEEK, 1)
        hedged_margin = weekly * sum(c.margin_share for c in flow.classes if c.kind == "hedged") * h_scale
        is_open = paths.is_open
        for mi in range(n_markets):
            for pi in range(n_paths):
                opens = np.flatnonzero(is_open[pi, :, mi])
                gaps = np.flatnonzero(np.diff(opens) >= 24)
                for g in gaps:
                    entry, reopen = opens[g], opens[g + 1]
                    leg = scenario.gap_pair_share * hedged_margin[pi, mi] / 2 / balance[0, mi]
                    move = paths.open[pi, reopen, mi] / close[pi, entry, mi] - 1.0
                    gain = abs(move)
                    loss = min(gain, balance[0, mi])
                    fee = float(accounting.profit_fee(gain))
                    fin = (prep.rollovers[reopen] - prep.rollovers[entry]) * fin_both[mi]
                    cash[pi, reopen, mi] += leg * (fee + fin - (gain - loss))
                    proceeds[pi, reopen, mi] += leg * (2 * balance[0, mi] - loss + gain - fee - fin)
                    margin[pi, entry, mi] += 2 * leg * balance[0, mi]
                    totals["fee"][pi] += leg * fee
                    totals["financing"][pi] += leg * fin
                    totals["bad_debt"][pi] += leg * (gain - loss)
                    totals["mint_basis"][pi] += leg * loss
                    totals["notional"][pi] += 2 * leg

    prep.pair_cash, prep.pair_proceeds, prep.pair_margin, prep.pair_totals = cash[:, :hours], proceeds[:, :hours], margin, totals


CHUNK_H = 24
# Ring channels: change in clients' quantity and cost, house cash, client proceeds.
Q, COST, HOUSE, PROCEEDS = range(4)
# Total channels: fee, financing, bad debt, mint basis.
FEE, FINANCING, BAD_DEBT, MINT = range(4)


def cohort_units(prep: Prepared, scenario: Scenario, markets: Markets, cls, side: int, t0: int, t1: int, hours: int):
    """Per unit of notional, the settlement of a directional cohort entering in each hour of
    [t0, t1): exit hour (paths, n, markets), ring channels (paths, n, markets, 4), and total
    channels (paths, n, markets, 4). A cohort settling past the horizon has zero channels."""
    s_i = 0 if side > 0 else 1
    j = cls.bucket
    balance = 1.0 / prep.leverage[j][None, None, :]
    paths = prep.paths
    t = np.arange(t0, t1)[None, :, None]
    target = np.minimum(np.arange(t0, t1) + cls.hold_h, hours)
    planned = prep.next_open[:, target, :]
    if not scenario.hold_through_closures:
        last = prep.prev_open[:, target, :]
        planned = np.where(last > t, last, planned)
    cross = prep.first_cross[s_i, j][:, t0:t1].astype(np.int64)
    closed_out = (cross > 0) & (t + cross <= planned)
    exit_hour = np.where(closed_out, t + cross, planned)
    p = np.arange(exit_hour.shape[0])[:, None, None]
    m = np.arange(exit_hour.shape[2])[None, None, :]
    at = np.minimum(exit_hour, hours - 1)
    entry = paths.close[:, t0:t1]
    trig = prep.trigger[s_i, j][None, None, :]
    bar_open = paths.open[p, at, m] / entry
    fill = np.where(side * (bar_open - trig) <= 0, bar_open, trig)
    x = np.where(closed_out, fill, paths.close[p, at, m] / entry)
    client, bad = accounting.settle(side, x, balance)
    fee = accounting.profit_fee(client)
    fin_rate = (markets.fin_long if side > 0 else markets.fin_short)[None, None, :]
    fin = (prep.rollovers[np.minimum(exit_hour, hours)] - prep.rollovers[t]) * fin_rate
    shortfall = np.maximum(fee + fin - balance - client, 0.0)  # financing past the balance
    valid = exit_hour < hours
    ring = np.stack([-side / entry, np.full_like(entry, -side), -client + fee + fin - shortfall, balance + client - fee - fin + shortfall], axis=-1)
    totals = np.stack([fee, fin, bad + shortfall, np.maximum(-client, 0.0)], axis=-1)
    return np.where(valid, exit_hour, t), ring * valid[..., None], totals * valid[..., None]


def run_batch(scenario: Scenario, markets: Markets, bars: np.ndarray, flow: Flow, floors: np.ndarray, total_floors: np.ndarray, seed: int, n_paths: int) -> dict:
    """Simulates `n_paths` paths for every lane: `floors` is (lanes, markets) and `total_floors`
    (lanes,). Returns per-(path, lane) results. State keeps lanes on the last axis, so a
    settlement scheduled for one (path, market) writes one contiguous block of lanes."""
    rng = np.random.default_rng(seed)
    prep = prepare(scenario, markets, bars, flow, rng, n_paths)
    hours = scenario.days * 24
    n_lanes = floors.shape[0]
    n_markets = len(markets.symbols)
    paths = prep.paths
    shape = (n_paths, n_markets, n_lanes)
    lanes_shape = (n_paths, n_lanes)
    ring_len = prep.horizon + 2
    market_floors = floors.T[None]  # (1, markets, lanes)
    cap_bps = markets.cap_bps[None, :, None]

    q = np.zeros(shape)  # clients' net quantity, in units of entry-relative price
    cost = np.zeros(shape)  # clients' net cost basis, USD
    house = np.zeros(shape)  # house cash by market
    ring = np.zeros((n_paths, ring_len, n_markets, 4, n_lanes))
    totals = np.zeros((n_paths, n_markets, 4, n_lanes))
    vault, idle, in_positions, queue = (np.zeros(lanes_shape) for _ in range(4))

    out = {k: np.zeros(lanes_shape) for k in ("desired", "refused", "desired_directional", "deposits", "withdrawn", "max_queue", "max_gross_exposure")}
    out["min_nav"] = np.full(lanes_shape, np.inf)
    out["min_cash"] = np.full(lanes_shape, np.inf)
    out["min_cover"] = np.full(lanes_shape, np.inf)  # vault USDC less every free balance and queued claim
    out["ever_negative"] = np.zeros(lanes_shape, dtype=bool)
    out["queued"] = np.zeros(lanes_shape, dtype=bool)
    out["first_negative_h"] = np.full(lanes_shape, -1)
    started = np.zeros(lanes_shape, dtype=bool)  # the house has cash or exposure
    out["daily_nav"] = np.zeros((n_paths, n_lanes, scenario.days), dtype=np.float32)
    refused_by_market = np.zeros(shape)
    desired_by_market = np.zeros((n_paths, n_markets))
    identity_gap = 0.0

    h_scale, d_scale = hedged_scale(scenario, flow)
    decay = farm_decay(scenario, hours)
    directional = [c for c in flow.classes if c.kind == "directional" and c.hold_h > 0]
    cohorts = [(cls, side) for cls in directional for side in (1, -1)]
    churn = [c for c in flow.classes if c.hold_h == 0]
    deviation = np.zeros((n_paths, 1 if scenario.common_direction else n_markets))
    log_close = np.log(paths.close)
    p_idx = np.arange(n_paths)[:, None]
    m_idx = np.arange(n_markets)[None, :]
    margin_unit = [(cls, cls.margin_share * d_scale * prep.leverage[cls.bucket]) for cls in directional]
    churn_unit = [(cls, cls.margin_share * prep.leverage[cls.bucket]) for cls in churn]

    for t0 in range(0, hours, CHUNK_H):
        t1 = min(t0 + CHUNK_H, hours)
        units = [cohort_units(prep, scenario, markets, cls, side, t0, t1, hours) for cls, side in cohorts]
        for t in range(t0, t1):
            k = t - t0
            slot = t % ring_len
            due = ring[:, slot]  # (paths, markets, channel, lanes)
            q += due[:, :, Q]
            cost += due[:, :, COST]
            exit_cash = due[:, :, HOUSE] + prep.pair_cash[:, t][..., None]
            house += exit_cash
            proceeds = due[:, :, PROCEEDS].sum(axis=1) + prep.pair_proceeds[:, t].sum(axis=1)[:, None]
            due[...] = 0.0

            entry = paths.close[:, t]
            exposure = entry[..., None] * q
            nav = (house - (exposure - cost)).sum(axis=1)
            cap = accounting.exposure_cap(nav[:, None, :], cap_bps, market_floors)
            total_cap = accounting.exposure_cap(nav, markets.total_cap_bps, total_floors[None])

            # Client long share: AR(1) around each class's measured share, leaning on the last return.
            prev_ret = log_close[:, t - 1] - log_close[:, t - 2] if t >= 2 else np.zeros((n_paths, n_markets))
            if scenario.common_direction:
                prev_ret = prev_ret.mean(axis=1, keepdims=True)
            deviation = flow.long_share_ar1 * deviation + flow.long_share_slope * prev_ret + flow.long_share_resid_sd * rng.standard_normal(deviation.shape)

            margin_hour = prep.margin_rate_hour[:, t]
            wants = []
            for cls, unit in margin_unit:
                notional = margin_hour * unit
                share = np.clip(cls.long_share + deviation, 0.0, 1.0)
                wants += [notional * share, notional * (1 - share)]
            zero = np.zeros((n_paths, n_markets))
            long_total = sum(wants[0::2], zero)
            short_total = sum(wants[1::2], zero)
            churn_long, churn_short, churn_hedged, gain_long, gain_short, gain_hedged = (np.zeros((n_paths, n_markets)) for _ in range(6))
            for cls, unit in churn_unit:
                notional = margin_hour * unit
                gain = GAIN_PER_SD * paths.parkinson[:, t] * cls.mean_sqrt_hold_h
                if cls.kind == "hedged":
                    churn_hedged += notional * h_scale * decay[t]
                    gain_hedged += notional * h_scale * decay[t] * gain
                else:
                    share = np.clip(cls.long_share + deviation, 0.0, 1.0)
                    churn_long += notional * d_scale * share
                    churn_short += notional * d_scale * (1 - share)
                    gain_long += notional * d_scale * share * gain
                    gain_short += notional * d_scale * (1 - share) * gain

            long_ok, short_ok = accounting.accepted_share(exposure, cap, long_total[..., None], short_total[..., None])
            change = long_ok * long_total[..., None] - short_ok * short_total[..., None]
            growth = np.maximum(np.abs(exposure + change) - np.abs(exposure), 0.0)
            room = np.maximum(total_cap - np.abs(exposure).sum(axis=1), 0.0)
            wanted = growth.sum(axis=1)
            cut = np.where(wanted > room, 1.0 - room / np.where(wanted > 0, wanted, 1.0), 0.0)[:, None, :] * growth
            with np.errstate(divide="ignore", invalid="ignore"):
                long_ok = np.where((change > 0) & (long_total[..., None] > 0), np.maximum(long_ok - cut / long_total[..., None], 0.0), long_ok)
                short_ok = np.where((change < 0) & (short_total[..., None] > 0), np.maximum(short_ok - cut / short_total[..., None], 0.0), short_ok)

            desired = long_total + short_total + churn_long + churn_short
            refused = (1 - long_ok) * (long_total + churn_long)[..., None] + (1 - short_ok) * (short_total + churn_short)[..., None]
            desired_by_market += desired
            refused_by_market += refused
            out["desired"] += (desired + churn_hedged).sum(axis=1)[:, None]
            out["desired_directional"] += desired.sum(axis=1)[:, None]
            out["refused"] += refused.sum(axis=1)

            # Book accepted directional cohorts: state and totals per side, settlement per cohort.
            need = np.zeros(lanes_shape)
            for side, ok in ((1, long_ok), (-1, short_ok)):
                mine = [(n, cls) for n, (cls, s) in enumerate(cohorts) if s == side]
                if not mine:
                    continue
                want = sum(wants[n] for n, _ in mine)
                q += side * ok * (want / entry)[..., None]
                cost += side * ok * want[..., None]
                need += (ok * sum(wants[n] / prep.leverage[cls.bucket] for n, cls in mine)[..., None]).sum(axis=1)
                totals += ok[:, :, None, :] * sum(wants[n][..., None] * units[n][2][:, k] for n, _ in mine)[..., None]
                for n, _ in mine:
                    exit_hour, ring_unit, _ = units[n]
                    ring[p_idx, exit_hour[:, k] % ring_len, m_idx] += ok[:, :, None, :] * (wants[n][..., None] * ring_unit[:, k])[..., None]

            # Churn pays the fee on its expected gain over its hold.
            churn_gain = gain_long[..., None] * long_ok + gain_short[..., None] * short_ok + gain_hedged[..., None]
            churn_fee = accounting.profit_fee(churn_gain)
            house += churn_fee
            totals[:, :, FEE] += churn_fee
            totals[:, :, MINT] += churn_gain

            # Vault: exit proceeds to idle balances, new margin drawn from them first and the rest
            # deposited, then withdrawals of what is left, and the queue.
            idle += proceeds - churn_fee.sum(axis=1)
            need += prep.pair_margin[:, t].sum(axis=1)[:, None]
            draw = np.minimum(np.maximum(idle, 0.0), need)
            idle -= draw
            vault += need - draw
            in_positions += need - proceeds - exit_cash.sum(axis=1)
            out["deposits"] += need - draw
            left = np.maximum(idle, 0.0)
            from_exits = scenario.withdraw_on_exit * np.minimum(np.maximum(proceeds, 0.0), left)
            idle_rate = (scenario.idle_withdraw_per_day + np.where(nav < 0, scenario.run_withdraw_per_day, 0.0)) / 24
            request = from_exits + idle_rate * (left - from_exits)
            idle -= request
            paid = np.where(queue > 0, 0.0, np.minimum(vault, request))
            queue += request - paid
            vault -= paid
            settle_queue = np.minimum(queue, vault)
            queue -= settle_queue
            vault -= settle_queue
            out["withdrawn"] += paid + settle_queue
            out["queued"] |= queue > 1e-6
            out["max_queue"] = np.maximum(out["max_queue"], queue)
            identity_gap = max(identity_gap, float(np.abs(vault - (idle + in_positions + queue + house.sum(axis=1))).max()))

            exposure = entry[..., None] * q
            nav = (house - (exposure - cost)).sum(axis=1)
            newly = (nav < 0) & ~out["ever_negative"]
            out["first_negative_h"] = np.where(newly, t, out["first_negative_h"])
            out["ever_negative"] |= nav < 0
            started |= (np.abs(house).sum(axis=1) > 0) | (np.abs(exposure).sum(axis=1) > 0)
            out["min_nav"] = np.minimum(out["min_nav"], np.where(started, nav, np.inf))
            out["min_cash"] = np.minimum(out["min_cash"], house.sum(axis=1))
            out["min_cover"] = np.minimum(out["min_cover"], np.where(started, house.sum(axis=1) + in_positions, np.inf))
            out["max_gross_exposure"] = np.maximum(out["max_gross_exposure"], np.abs(exposure).sum(axis=1))
            if t % 24 == 23:
                out["daily_nav"][:, :, t // 24] = nav

    for name, channel in (("fee", FEE), ("financing", FINANCING), ("bad_debt", BAD_DEBT), ("mint_basis", MINT)):
        out[name] = totals[:, :, channel].sum(axis=1) + prep.pair_totals[name][:, None]
    out["desired"] += prep.pair_totals["notional"][:, None]
    out["nav_end"] = nav
    out["pnl_by_market"] = (house - (exposure - cost)).transpose(0, 2, 1)
    out["refused_by_market"] = refused_by_market.transpose(0, 2, 1)
    out["desired_by_market"] = np.broadcast_to(desired_by_market[:, None, :], (n_paths, n_lanes, n_markets)).copy()
    out["identity_gap"] = np.array(identity_gap)
    return out


def run(scenario: Scenario, markets: Markets, bars: np.ndarray, flow: Flow, floors: np.ndarray, total_floors: np.ndarray, workers: int = 1) -> dict:
    """All paths of a scenario, in batches, concatenated by path."""
    return run_many([(scenario, markets, bars, flow, floors, total_floors)], workers)[0]


def run_many(tasks: list[tuple], workers: int = 1) -> list[dict]:
    """Runs several (scenario, markets, bars, flow, floors, total_floors) tasks, splitting each
    into batches of paths and running every batch in one pool of `workers` processes."""
    jobs, owner = [], []
    for n, (scenario, markets, bars, flow, floors, total_floors) in enumerate(tasks):
        for i, start in enumerate(range(0, scenario.paths, scenario.batch_paths)):
            size = min(scenario.batch_paths, scenario.paths - start)
            jobs.append((scenario, markets, bars, flow, floors, total_floors, scenario.seed * 100_003 + i, size))
            owner.append(n)
    if workers > 1:
        from concurrent.futures import ProcessPoolExecutor

        with ProcessPoolExecutor(max_workers=workers) as pool:
            parts = list(pool.map(_run_job, jobs))
    else:
        parts = [_run_job(job) for job in jobs]
    results = []
    for n in range(len(tasks)):
        mine = [part for part, o in zip(parts, owner) if o == n]
        merged = {}
        for key in mine[0]:
            if key == "identity_gap":
                merged[key] = max(float(p[key]) for p in mine)
            else:
                merged[key] = np.concatenate([p[key] for p in mine], axis=0)
        results.append(merged)
    return results


def _run_job(job):
    return run_batch(*job)
