# Cap calibration

Draft: the per-market sweeps are in `results/report.md`. The joint runs that set the recommended
floors, and the sensitivities, are still running; their results and the summary go here.

## The question

The house is the counterparty to every trade and starts with no capital (#15 §2). Each exposure
cap, per market and in total, is the larger of a share of house NAV and a fixed dollar floor (#45,
PR #99). At launch, NAV is near zero, so the floors are the whole of the house's exposure: they
decide how much unbacked risk the house carries before trader losses have built its capital, and
how many orders it refuses while it is small (#111).

This study estimates, for each market and in total, how the floors trade the probability of house
insolvency over the first 90 days against the share of orders refused for capacity, from
papertrade.xyz's on-chain trader behaviour and the launch markets' price history. It recommends
floors at three insolvency targets and reports the sensitivity to the main assumptions. The values
are not applied to `config/production-genesis.toml`.

## Insolvency measures

Each is a probability over 90 days of simulated trading, starting from a house with no capital.

| Measure | Event |
|---|---|
| NAV below zero, ever | House NAV (house cash less clients' unrealised PnL) is below zero at some hour-end. |
| Vault short of free balances | At some hour-end the vault holds less USDC than the free balances clients could withdraw on demand: house cash plus the margin in open positions is negative. This is NAV below the withdrawals owed. |
| Queue | Under the modelled withdrawals, some withdrawal waits in the #49 first-in-first-out queue because the vault cannot pay it. |
| NAV below zero at day 90 | House NAV is below zero at the end. |
| NAV below -$100k, -$1M | The lowest NAV falls below that depth. |

The calibration uses the first two. Below 1% (20 of 2,000 paths), a probability is extrapolated
from an exponential fit to the worst 5% of paths' lowest NAV or lowest cover; when that fit does
not apply and no path fails, it is the 95% upper bound for zero events, 3 in the number of paths.

## Data

### papertrade.xyz trader behaviour

papertrade.xyz runs a house-counterparty perpetuals venue on Hyperliquid's EVM (HyperEVM, chain
999): an LP that starts at zero takes the other side of every trade, losers pay nothing beyond
their loss, winners pay a 2% fee after an impact haircut that keeps at most 90% of a gain, and
losses mint the PAPER token, 100 per $1 while the LP is below $2M. It lists BTC and ETH at up to
1000:1 with isolated margin and a $10M position cap ([docs](https://docs.papertrade.xyz),
[contract addresses](https://docs.papertrade.xyz/dev/contract-addresses/),
[impact haircut](https://docs.papertrade.xyz/how/asymmetric-impact/),
[mint curve](https://docs.papertrade.xyz/how/mint-curve/)).

`fetch/papertrade.py` pulls every event log of the Exchange proxy
`0x6cd5661646289fb6e65ea5c032310fded797d0a2` from its creation block 47,854,719 to block
49,108,718 (2026-10-21 05:25 UTC) from six public HyperEVM RPC endpoints, 1,254 chunks of 1,000
blocks, and block timestamps every 10,000 blocks. 25 randomly chosen chunks, fetched again from an
endpoint chosen at random, matched their stored log counts. The raw pull is 212 MB and gitignored.

The Exchange's source is not verified on the explorer, so `derive/papertrade.py` infers the event
layouts from the logs and checks them:

- every one of the 1,580,269 closes and 132,923 liquidations matches one of the 1,715,360 opens;
- each close's fee is 2% of its haircut PnL, the documented win fee and LP-side loss fee
  (median ratio 0.0200 over 1,374,753 closes with a fee);
- BTC entry prices sit within 0.1% (median) of Binance's BTCUSDT close of the same hour;
- deposits ($351.2M) less withdrawals ($305.9M) give $45.3M, against the $45.36M of USDC the
  explorer showed the Exchange holding on 2026-10-21.

The contract's open-interest event is not used: its changes do not follow position flows, and
until block 48,217,087 it carried a fixed offset of about $5B a side.

Trading opened at 2026-10-10 14:00 UTC, so the data covers 10.6 days: 18,504 accounts and $713B
of notional. A position is *hedged* when its account already held at least as much opposite
notional in the same asset, or when an account of the same notional took the opposite side within
two blocks of its open and close. The derived dataset, committed under `data/derived/`, is the
flow profile the model samples (`papertrade_flow.csv`: margin by kind, hold class, and leverage
bin), the daily series (`papertrade_daily.csv`), the holding-time table (`papertrade_holding.csv`),
and the summary (`papertrade_summary.json`). `results/report.md` tabulates them.

What the data says about the brief's farming premise:

- Hedged positions are 31% of notional and 50% of exposure-time; within accounts, 60% of
  gross exposure-time is offset by the same account's opposite positions, and market-wide 87%.
- Holds are short: the median position lasts 64 seconds, but 71% of exposure-time comes from
  positions held over an hour and 9% from positions held over a day.
- Hedged flow pays $0.232 of haircut and fee on its winning legs per $1 lost on its losing legs.
  On this engine a winning leg pays a 10% profit fee and no haircut, so the same farming costs about
  $0.10 per $1 of loss, and the house takes less than half of what papertrade takes from it.
- Flow fell from $373M of margin on the first full day to $79M on the last.

### Launch markets

`config/production-genesis.toml` lists 37 markets. `fetch/markets.py` pulls, for each:

| Data | Source |
|---|---|
| Hourly OHLC, 2024-10-28 to 2026-10-12 (102 weeks) | Crypto: Binance spot klines (`data-api.binance.vision`). Everything else: Yahoo Finance chart API (`query1.finance.yahoo.com/v8/finance/chart`), which serves hourly bars for the last 730 days. |
| Daily closes from 2016 | FRED (St. Louis Fed) where an official series exists: `DEXUSEU`, `DEXUSUK`, `DEXJPUS`, `DEXUSAL`, `SP500`, `NASDAQ100`, `DJIA`, `NIKKEI225`, `DCOILWTICO`, `DCOILBRENTEU`. Otherwise Binance daily klines or Yahoo daily bars, dividend-adjusted for shares. |
| Benchmark rates | FRED `SOFR` (3.87%), `IUDSOIA` (SONIA, 3.73%), `ECBESTRVOLWGTTRMDMNRT` (ESTR, 2.44%), each on 2026-10-19 or the latest before; TONA 0.75% and AONIA 3.60% are assumptions. |
| Market mix | Hyperliquid's public info API (`api.hyperliquid.xyz/info`, daily candles 2026-09-20 to 2026-10-19): each market's mean daily notional on its Hyperliquid listing, main dex or the builder-deployed `xyz` dex. |

Each launch market's stand-in series (`catalogue.py`): Binance BTCUSDT, ETHUSDT, SOLUSDT; spot FX;
CME E-mini S&P 500, Nasdaq-100 and Dow front months for US500, US100 and US30, as the venue
calendars use; CME's Nikkei yen future for JP225; COMEX gold and silver and NYMEX WTI and Brent
front months; the shares' regular sessions. DE40 and UK100 use the DAX and FTSE 100 cash indices,
as the venue futures are not public.

`derive/markets.py` cleans the hourly bars:

- A gap across a one-hour step is set to zero. The public futures series alternates between
  front- and next-month contracts around a roll (consecutive bars jump by ±80 to 170 bp and back),
  and a continuously traded market's next bar opens at the previous close.
- For crypto, FX, and futures, a gap after an unexplained weekday hole of 3 to 24 hours is set to
  zero: the futures series switches contract at such holes (ES jumped 2.6% across one on
  2024-12-20). Gaps over the daily break, weekends, and holidays are kept.
- DE40 and UK100 fold each weekday overnight gap into continuous movement, since their venue
  futures trade 20 or more hours a day, and keep weekend gaps, taken to the first bar's close
  because the FTSE series opens at the previous close.

`market_stats.csv` gives each market's volatility, the reopening gaps a year past half its first
margin tier (where a position at the maximum leverage is closed out) and past the whole tier (where
it loses more than its balance), and the largest hourly and daily moves. `results/report.md` shows
it in full. The hourly window is a high-volatility one for metals and several shares (gold 26% a
year against 17% since 2016, silver 57% against 35%) and a calm one for crypto (BTC 44% against
67%).

## Model

`run.py` runs the Monte Carlo (`model/`) hour by hour over 90 days, from a house with no capital.

**Prices.** A path is 13 historical weeks, drawn in blocks of four consecutive weeks from the 102,
the same weeks for every market. That keeps cross-market correlation, volatility clusters,
weekend and holiday gaps, and each venue's sessions. Each market's mean hourly return over the
window is removed, so trends in the history do not decide who wins.

**Flow.** papertrade's flow is carried over by margin, the collateral traders commit, not by
notional, because these markets allow 10:1 to 200:1, not 1000:1. Each day brings papertrade's
margin of the same day after launch (its ten full days in order, then the mean of the last seven,
$182M a day). Each market takes its Hyperliquid volume share of it (crypto 76.5%, indices 8.8%,
oil 6.0%, metals 3.1%, shares 5.5%, FX 0.1%), spread over its open hours. A market with no listing
takes its asset class's smallest listed share. Each papertrade class keeps its share of margin,
hold, and long share. Its leverage is capped at the market's maximum, the first margin tier, so
most flow trades at the maximum.

- *Directional* positions open at the hour's close and settle after their class's hold (1, 4, 16,
  or 48 hours); a planned exit inside a closure waits for the reopening. The long share follows an
  hourly AR(1) around each class's measured share, fitted to papertrade's BTC flow (coefficient
  0.70, residual 0.11, and +0.40 per unit of the last hour's log return).
- *Hedged* positions open as offsetting pairs in two accounts, since an engine account nets to
  one position per market. A pair never changes the house's net exposure, so the caps never
  refuse it. It ends when either leg is closed out, the other leg closing with it, or at its hold.
- Positions held under an hour (churn: 87% of margin, 93% of positions) pay the profit fee on their
  expected gain over their hold, at the hour's Parkinson volatility.

**Engine rules** (`model/accounting.py`, each against the engine function it mirrors; the tests
check them on hand cases from the engine's own tests):

- a 10% profit fee on realised gains, nothing on losses (`profit_fee`);
- financing at each 22:00 rollover: longs pay benchmark + markup, shorts markup - benchmark, at
  the genesis markups and day bases (`daily_financing`);
- close-out when equity falls below 50% of margin, at the trigger in continuous trading or at the
  bar's open when the market gaps through it, only while the market is open (`closeout.rs`);
- negative balance protection: a loss beyond the account's balance is bad debt against the house
  (`settle_bad_debt`);
- USD settlement (#98): USD-JPY, Japan 225 (yen), Germany 40 (euros), and UK 100 (pounds) accrue
  PnL in their quote currency and convert it at the engine's own USD-JPY, EUR-USD, or GBP-USD
  price, from the same bootstrapped weeks, so their margin, close-outs, caps, and NAV are in USD;
- each cap the larger of its genesis NAV share (`house_cap_bps`, `house_total_cap_bps`) and its
  floor; an order that would take the house's net exposure past a cap is refused (`check_risk`).
  Over an hour, the side that reduces exposure is accepted in full and the crowded side up to the
  cap.

**MONKEY.** Losing trades mint MONKEY at a rate per dollar of realised loss (#36), but genesis
sets neither the rate nor the half-life, and MONKEY moves no house cash. The model reports the
loss basis it mints on. The incentive enters through the hedged flow, which the farming share
scales, and a half-life on it is a sensitivity.

**Vault and withdrawals.** Exit proceeds go to clients' free balances. New margin is drawn from
free balances first and deposited for the rest. Half of each hour's remaining exit proceeds is
withdrawn, and free balances leave at 5% a day, or 55% a day while NAV is below zero (a run on a
public log). A withdrawal the vault cannot pay joins the #49 queue and is paid as deposits arrive.

**Calibration.** Each market alone, at its share of the flow, gives each measure as a function of
its floor, over 18 floors from $0 to $100M. A *risk level* `q` sets every market's floor to the
largest floor at which that market alone stays at or below `q`. That floor is capped where the
market stops refusing orders (under 1% refused) and is zero when even a zero floor exceeds `q`. The
total floor is the sum. All markets together are then run at nine levels, and a target takes the
highest level whose joint measure meets it.

## Results

`results/report.md` holds every table: the data summaries, each market alone, all markets
together at each risk level with the floors chosen per target, and the sensitivities.

## Limits

- **Ten days of behaviour.** The papertrade data covers 10.6 days of a new venue: two BTC/ETH
  markets at 1000:1, crypto-native users, and a launch spike. Holds, leverage, and long share on
  RWA markets, at lower leverage and with closures, are assumptions carried over from it.
- **Fixed holds.** Each hold class settles at one hold, 1, 4, 16, or 48 hours; papertrade's
  notional-weighted mean holds within the classes are 1.4, 3.7, 13.7, and 53.6 hours.
- **Volume.** The flow level assumes this engine draws papertrade's margin. Fees scale with it, so
  the `flow x0.25` and `flow x4` rows bound the effect.
- **Two years of hourly history.** The tails come from 102 weeks (late 2024 to late 2026), which
  include April 2025 and March 2026 but not March 2020. Probabilities of 0.1% and below are
  extrapolations from the worst 5% of paths, not counts; the volatility rows stress them.
- **Price stand-ins.** DE40 and UK100 use cash indices. Futures series carry roll artefacts that
  the cleaning removes only at holes and one-hour steps; a roll that coincides with a weekend stays
  in that weekend's gap, which overstates gaps for WTI and Brent. Yahoo's hourly bars are a public
  but unofficial source.
- **Engine detail left out.** Size spreads, stress spreads, and toxicity spreads would add house
  income, and leaving them out is conservative. Further gaps: cross-margin between an account's
  markets (each position is its own account), margin bands above the first tier, close-outs
  triggered by financing, the stale-price rule that stops dealing when venues disagree (#15 §1),
  and the zero-mean PnL of sub-hour churn.
- **Close-out fills.** In continuous trading a close-out fills at its trigger. A crash inside an
  hour that the venue index jumps across fills worse in the engine, so crypto bad debt is
  understated.
- **Withdrawals.** The withdrawal and run rates are assumptions; the queue measure depends on
  them, and the shortfall measure does not.

## Re-running

From a virtualenv outside the repo (Python 3.12 or newer):

```sh
python3 -m venv /tmp/cap-venv && /tmp/cap-venv/bin/pip install -r research/cap-calibration/requirements.txt
cd research/cap-calibration
/tmp/cap-venv/bin/python -I -m pytest tests     # the accounting tests
/tmp/cap-venv/bin/python -I run.py all          # per-market, joint, sensitivity, then results/report.md
```

`all` reruns everything from the committed `data/derived/`; it takes a few hours on 4 to 10
workers (`--workers`), and `--batch-paths` trades memory for speed. Stages run alone with
`per-market`, `joint`, `sensitivity`, and `report`. Each batch of paths is seeded from `--seed`
(default 1) and its index, so the same seed and batch size reproduce a run exactly;
`results/manifest_<stage>.json` records the scenario, including both, and the hashes of the
derived files it read.

To rebuild the data: `fetch-papertrade` (resumes; `--to-block` pins the end), `fetch-markets`,
`derive-papertrade`, `derive-markets`. Yahoo serves hourly bars for the last 730 days only, so a
later rebuild needs a later window in `fetch/markets.py`.

To recalibrate as live data arrives:

- replace papertrade's flow with this engine's own: the public log gives every order, fill, and
  close, from which the same flow profile (margin by hold class and leverage, long share, hedged
  share) follows, and the market mix becomes the engine's own volume;
- move the price window forward and widen it as hourly history allows;
- set the MONKEY rate and half-life once #36 fixes them, and the benchmark rates from #33;
- rerun `all`, and change floors through the operator channel (#47).

## Files

- `run.py`: the entry point.
- `catalogue.py`: launch markets from the genesis, and the public series for each.
- `fetch/`: `papertrade.py` and `markets.py` pull the raw data into `data/raw/` (gitignored).
- `derive/`: `papertrade.py` decodes positions; `markets.py` builds the hourly dataset, statistics,
  and mix, into `data/derived/`.
- `model/`: `accounting.py` (engine rules), `prices.py` (bootstrap), `flow.py` (behaviour),
  `simulate.py` (the hourly simulation), `experiments.py` (runs and measures), `report.py`.
- `tests/`: the accounting and estimator tests.
- `results/`: the stage outputs, `report.md`, and run manifests.
