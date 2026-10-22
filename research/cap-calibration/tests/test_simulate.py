"""The simulation's accounting on one deterministic week of one market, in USD or quoted in
another currency.

The history is a single week, so every path replays it. Each hour 1 USD of margin opens a long at
200:1 (200 of notional, a balance of 1) that the model closes an hour later.
"""

import numpy as np
import pytest

from model import accounting, prices, simulate
from model.flow import Flow, FlowClass

LEVERAGE = 200.0
TINY_ORDERS = (np.array([1e-6, 1e9]), np.array([1.0, 1.0]))  # every order fits any positive room


def bars(gap_at: int | None = None, gap: float = 0.0, fx_gap_at: int | None = None, fx_gap: float = 0.0, closed: tuple = (), hour_moves: dict | None = None) -> np.ndarray:
    """One week of a flat market (column 0) and of its FX market (column 1), each with an
    optional gap at the open of one hour; `closed` hours of the market, and `hour_moves` as
    {hour: (high, low, close)} log moves of the market's bar against its open."""
    b = np.zeros((prices.WEEK, 2, 4), dtype=np.int32)
    for hour, (high, low, close) in (hour_moves or {}).items():
        b[hour, 0, 1:] = [round(high * 1e6), round(low * 1e6), round(close * 1e6)]
    for hour in closed:
        b[hour, 0] = prices.CLOSED
    if gap_at is not None:
        b[gap_at, 0, 0] = round(np.log1p(gap) * 1e6)
    if fx_gap_at is not None:
        b[fx_gap_at, 1, 0] = round(np.log1p(fx_gap) * 1e6)
    return b


def market(fin: float = 0.0, quoted_in_fx: bool = False) -> simulate.Markets:
    return simulate.Markets(
        symbols=["TEST"],
        margin_rate=np.array([1 / LEVERAGE]),
        cap_bps=np.array([5000]),
        total_cap_bps=20000,
        fin_long=np.array([fin]),
        fin_short=np.array([fin]),
        mix=np.array([1.0]),
        open_hours_per_week=np.array([168.0]),
        columns=np.array([0]),
        fx_columns=np.array([1 if quoted_in_fx else -1]),
        fx_invert=np.array([False]),
    )


def flow(kind: str = "directional", hold_h: int = 1, mean_hold_h: float | None = None) -> Flow:
    mean = float(hold_h) if mean_hold_h is None else mean_hold_h
    return Flow(
        classes=[FlowClass(kind=kind, hold_h=hold_h, bucket=0, margin_share=1.0, mean_hold_h=mean, mean_sqrt_hold_h=np.sqrt(mean), long_share=1.0)],
        bucket_leverage=np.array([LEVERAGE]),
        daily_margin=np.array([24.0]),
        sizes={(k, m, int(LEVERAGE)): TINY_ORDERS for k in ("directional", "hedged") for m in ("capped", "relative")},
        long_share_ar1=0.0,
        long_share_resid_sd=0.0,
        long_share_slope=0.0,
    )


def run(b, m, f, floor=1e12, total_floor=1e12, **overrides):
    settings = dict(paths=1, batch_paths=1, days=7, block_weeks=1, demean=False, withdraw_on_exit=0.0, idle_withdraw_per_day=0.0, run_withdraw_per_day=0.0)
    scenario = simulate.Scenario(**(settings | overrides))
    out = simulate.run_batch(scenario, m, b, f, np.array([[floor]]), np.array([total_floor]), seed=0, n_paths=1)
    return {k: v[0, 0] if np.ndim(v) >= 2 else v for k, v in out.items()}


def test_a_flat_path_conserves_cash():
    out = run(bars(), market(), flow())
    assert out["nav_end"] == pytest.approx(0.0, abs=1e-9)
    assert out["fee"] == out["bad_debt"] == out["financing"] == 0.0
    assert out["refused"] == 0.0
    assert float(out["identity_gap"]) < 1e-9
    # Margin is recycled: the first hour's deposit funds every later position.
    assert out["deposits"] == pytest.approx(1.0)


def test_financing_is_the_only_income_on_a_flat_path():
    rate = 1e-4
    out = run(bars(), market(fin=rate), flow())
    # 200 of notional is open across each 22:00 rollover; 7 rollovers in the week.
    assert out["financing"] == pytest.approx(7 * 200 * rate)
    assert out["nav_end"] == pytest.approx(out["financing"])


def test_a_gap_through_the_balance_is_bad_debt():
    out = run(bars(gap_at=50, gap=-0.01), market(), flow())
    # The long opened at hour 49 is closed out at the 1% gap: it loses its balance of 1 and the
    # other 1 of the 2 loss is written off.
    assert out["bad_debt"] == pytest.approx(1.0, rel=1e-4)
    assert out["mint_basis"] == pytest.approx(1.0, rel=1e-4)
    assert out["nav_end"] == pytest.approx(1.0, rel=1e-4)
    assert out["fee"] == 0.0


def test_a_winning_close_pays_the_profit_fee():
    out = run(bars(gap_at=50, gap=0.01), market(), flow())
    # The long opened at hour 49 closes at hour 50, 1% up: a gain of 2 that pays 0.2.
    assert out["fee"] == pytest.approx(0.2, rel=1e-4)
    assert out["nav_end"] == pytest.approx(-1.8, rel=1e-4)


def test_the_cap_refuses_the_crowded_side_past_the_floor():
    out = run(bars(), market(), flow(), floor=100.0)
    # Each hour wants 200 long against a 100 cap at zero NAV: half is refused, in the first week too.
    assert out["refused"] / out["desired"] == pytest.approx(0.5)
    assert out["refused_week1"] / out["desired_week1"] == pytest.approx(0.5)


def test_a_pair_never_moves_exposure_and_gaps_cost_the_house():
    out = run(bars(gap_at=50, gap=-0.01), market(), flow("hedged"))
    assert out["max_gross_exposure"] == 0.0
    # Legs of 100 each: the long loses its balance of 0.5 and 0.5 more is bad debt, the short
    # gains 1 and pays 0.1.
    assert out["bad_debt"] == pytest.approx(0.5, rel=1e-4)
    assert out["fee"] == pytest.approx(0.1, rel=1e-4)
    assert out["nav_end"] == pytest.approx(-0.4, rel=1e-4)


def test_a_shortfall_queues_the_withdrawal_the_vault_cannot_pay():
    out = run(bars(gap_at=50, gap=0.01), market(), flow(), withdraw_on_exit=1.0)
    # The winner is owed 2.8 (balance 1 plus 1.8 net profit). Its 1 funds the next position and
    # the vault holds that position's 1, so 1 is paid and 0.8 waits.
    assert out["queued"]
    assert out["max_queue"] == pytest.approx(0.8, rel=1e-4)
    assert float(out["identity_gap"]) < 1e-9


def test_the_shortfall_is_house_cash_below_client_balances():
    # After the winning close the house has paid out 1.8 it never had.
    out = run(bars(gap_at=50, gap=0.01), market(), flow())
    assert out["min_cash"] == pytest.approx(-1.8, rel=1e-4)
    assert run(bars(), market(), flow())["min_cash"] == pytest.approx(0.0, abs=1e-12)


def test_nothing_opens_without_room_under_a_cap():
    # At zero NAV and a zero floor every cap is zero, so no order of any size fits.
    for kind in ("directional", "hedged"):
        out = run(bars(gap_at=50, gap=-0.01), market(), flow(kind), floor=0.0)
        assert out["deposits"] == 0.0
        assert out["nav_end"] == 0.0
    assert run(bars(), market(), flow(), floor=0.0)["refused"] / run(bars(), market(), flow(), floor=0.0)["desired"] == pytest.approx(1.0)


def test_orders_larger_than_the_cap_are_refused_whatever_the_flow_around_them():
    # Half the margin sits in orders of 200 of notional, half in orders of 2,000: under a 1,000 cap
    # only the small ones can fill.
    sizes = (np.array([199.9, 200.0, 1999.9, 2000.0]), np.array([0.0, 0.5, 0.5, 1.0]))
    f = flow()
    f.sizes = {key: sizes for key in f.sizes}
    out = run(bars(), market(), f, floor=1000.0)
    assert out["refused"] / out["desired"] == pytest.approx(0.5)


def test_a_currency_move_alone_leaves_a_quoted_position_flat():
    # Settled in its quote currency, a position carries FX risk on its PnL only, not its notional.
    out = run(bars(fx_gap_at=50, fx_gap=0.10), market(quoted_in_fx=True), flow())
    assert out["nav_end"] == pytest.approx(0.0, abs=1e-9)
    assert out["fee"] == 0.0


def test_a_quoted_gain_converts_at_the_fx_of_the_fill():
    out = run(bars(gap_at=50, gap=0.01, fx_gap_at=50, fx_gap=0.10), market(quoted_in_fx=True), flow())
    # The long opened at hour 49 gains 1% of 200 in the quote currency, worth 2.2 at the new FX.
    assert out["fee"] == pytest.approx(0.22, rel=1e-4)
    assert out["nav_end"] == pytest.approx(-1.98, rel=1e-4)


def test_a_stronger_quote_currency_brings_a_close_out_forward():
    # A 200:1 long that falls 0.23% in its quote currency keeps 0.27% of equity against a 0.25%
    # close-out level. A quote currency 10% stronger in USD turns the fall into 0.253% and lifts the
    # requirement 10%, so the same bar closes it out.
    def paths(fx_move: float) -> prices.Paths:
        ones = np.ones((1, 3, 1))
        low = ones.copy()
        low[0, 1, 0] = 1 - 0.0023
        fx = ones.copy()
        fx[0, 1:, 0] = 1 + fx_move
        return prices.Paths(ones, ones, low, ones, np.ones((1, 3, 1), dtype=bool), np.zeros((1, 3, 1)), fx)

    balance, margin_rate = np.array([[0.005]]), np.array([0.005])
    assert simulate.first_crossings(paths(0.0), balance, margin_rate, 2)[0, 0, 0, 0, 0] == 0
    assert simulate.first_crossings(paths(0.10), balance, margin_rate, 2)[0, 0, 0, 0, 0] == 1
    # The fill price for the close-out uses the same scaled balance.
    x = accounting.closeout_trigger(1, 0.005 / 1.1, 0.005)
    assert 0.005 + (x - 1) * 1.1 == pytest.approx(0.5 * 0.005 * x * 1.1)


def test_the_cap_measures_a_quoted_market_in_usd():
    # Positions held 4 hours carry exposure into later cap checks. Valued in USD, a position opened
    # after the currency gains 10% fills the 100 cap as it would with no currency move.
    quoted = run(bars(fx_gap_at=10, fx_gap=0.10), market(quoted_in_fx=True), flow(hold_h=4), floor=100.0)
    usd = run(bars(), market(), flow(hold_h=4), floor=100.0)
    assert usd["refused"] / usd["desired"] > 0.5
    assert quoted["refused"] == pytest.approx(usd["refused"])


def test_churn_pays_the_fee_on_its_expected_gain_and_its_dues_are_funded():
    # Every hour ranges 1% either side; churn held a quarter hour pays 10% of its expected gain.
    moves = {h: (0.01, -0.01, 0.0) for h in range(prices.WEEK)}
    out = run(bars(hour_moves=moves), market(), flow(hold_h=0, mean_hold_h=0.25))
    sigma = 0.02 / np.sqrt(4 * np.log(2))
    expected = 0.1 * simulate.GAIN_PER_SD * sigma * np.sqrt(0.25) * 200 * prices.WEEK
    assert out["fee"] == pytest.approx(expected, rel=1e-6)
    assert out["deposits"] == pytest.approx(expected, rel=1e-6)  # churners bring what they pay
    assert float(out["identity_gap"]) < 1e-9


def test_churn_open_notional_takes_the_hours_move():
    # Long churn held half an hour, 200 an hour: the 1% move at hour 50 costs the house 1.
    out = run(bars(gap_at=50, gap=0.01), market(), flow(hold_h=0, mean_hold_h=0.5))
    assert out["nav_end"] == pytest.approx(-0.5 * 200 * 0.01, rel=1e-3)


def test_the_total_cap_cuts_growth_past_its_room():
    out = run(bars(), market(), flow(), floor=1e12, total_floor=100.0)
    assert out["refused"] / out["desired"] == pytest.approx(0.5)


def test_exiting_before_a_closure_avoids_its_reopening_gap():
    # Closed 30 to 40, reopening 1% down. Held through, the 4-hour longs opened at 26 to 29 all
    # lose their balance to the gap; exiting before the close, only the one opened at 29 does.
    b = bars(gap_at=41, gap=-0.01, closed=tuple(range(30, 41)))
    through = run(b, market(), flow(hold_h=4))
    before = run(b, market(), flow(hold_h=4), hold_through_closures=False)
    assert through["bad_debt"] == pytest.approx(4.0, rel=1e-3)
    assert before["bad_debt"] == pytest.approx(1.0, rel=1e-3)


def test_pairs_held_through_a_closure_lose_the_gap_past_their_balance():
    # Pairs at 200:1 opened at hour 29, the last before a closure, lose 0.5% of each leg past the
    # loser's balance on a 1% reopening gap. Each leg is half the stressed share of the hedged
    # margin accepted over about the past week: 1 an hour, decaying with a one-week constant.
    b = bars(gap_at=41, gap=-0.01, closed=tuple(range(30, 41)))
    base = run(b, market(), flow("hedged"))
    stressed = run(b, market(), flow("hedged"), gap_pair_share=0.5)
    decay = np.exp(-1 / prices.WEEK)
    week_margin = (1 - decay**29) / (1 - decay)
    leg = 0.5 * week_margin / 2 * LEVERAGE
    assert stressed["bad_debt"] - base["bad_debt"] == pytest.approx(leg * 0.005, rel=1e-4)
    # Under a 1,000 cap that half the orders fit, half the past week's margin was accepted and half
    # of the stressed pairs fit now: a quarter of the uncapped stress.
    sizes = (np.array([199.9, 200.0, 1999.9, 2000.0]), np.array([0.0, 0.5, 0.5, 1.0]))
    f = flow("hedged")
    f.sizes = {key: sizes for key in f.sizes}
    capped = run(b, market(), f, floor=1000.0, gap_pair_share=0.5)["bad_debt"] - run(b, market(), f, floor=1000.0)["bad_debt"]
    assert capped == pytest.approx(0.25 * leg * 0.005, rel=1e-4)


def test_margin_in_positions_matches_the_positions_left_open():
    out = run(bars(), market(), flow(hold_h=4))
    assert out["in_positions_end"] == pytest.approx(4.0)  # the four cohorts opened at hours 164-167


def test_financing_is_charged_at_each_rollover_on_the_marked_notional():
    # 4-hour longs, 200 each, and a 10% rise at hour 20. At the 22:00 rollover the two opened
    # before the rise are worth 220 each and the two after it 200; at the six later rollovers,
    # four positions of 200.
    rate = 1e-4
    out = run(bars(gap_at=20, gap=0.10), market(fin=rate), flow(hold_h=4))
    assert out["financing"] == pytest.approx((2 * 220 + 2 * 200 + 6 * 4 * 200) * rate, rel=1e-6)
