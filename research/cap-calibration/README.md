# Cap calibration

## Summary

Two measures of insolvency set very different floors, and the owner's choice between them decides
the launch floors. Over 90 days from a house with no capital, on 2,000 simulated paths:

| Target | NAV never below zero | House cash never below zero (the vault covers client balances) |
|---|---|---|
| 1% | $37M in total: $1M on every market (0.84%). Refuses 50% of launch-week opening notional, 25% over 90 days. | $370M in total: $10M on every market (0.60%). Refuses 0.1% in the launch week. |
| 0.1% | $562k in total: BTC-USD $562k, every other market $0 (0.05%). Refuses 83% in the launch week, 39% over 90 days. | $2.79M in total: BTC-USD $1.78M, ETH-USD $1M, SOL-USD $10k, every other market $0 (below 0.01%). Refuses 73% in the launch week. |
| 0.01% | $316k in total: BTC-USD $316k, every other market $0 (below 0.01%, extrapolated). Refuses 86% in the launch week. | $0: nothing opens. |

The 1% row for NAV comes from the sensitivity stage's baseline at x0.1 (1,000 paths). The
risk-level method below
picks $562k on BTC-USD alone for it, refusing 83% of launch-week notional. That method sets each
market's floor from the market run alone, where a small market's own fees cannot absorb its gap
losses. With every market at $1M, pooled crypto fees carry the small markets, and the same target
refuses 50%. No uniform floor below $1M was run, so 0.1% and 0.01% keep the method's choice.

- **Early dips are common, shallow, and short.** With $10M on every market, NAV
  falls below zero at some hour in 7.9% of paths, at a median of 0.2 days after launch, before fees
  have built any capital. It falls below -$100k in 0.9% of paths and never below -$1M. House cash
  goes below zero in 0.6%, no withdrawal ever waits in the #49 queue, and NAV ends the 90 days at a
  median of $29M.
- **Order size, not exposure, is what small floors refuse.** An order larger than the cap is
  refused whatever flows around it. At 200:1, orders up to $1M carry only 40% of papertrade's
  margin, so a floor under a few million refuses most launch flow; $10M, papertrade's position cap,
  admits all of it.
- **Along the risk-level allocations, the dip rises quickly with the floors.** From 0.25% at $2.6M
  in total (8 markets with floors), it reaches 4.3% at $5.3M, 6.0% at $8.9M, and 7.4% at $143M. The
  method's 1% choice stops at $562k because the level at $1.29M measures 1.05%, inside sampling error
  of the target; $2.6M at 0.25% also meets it.
- **Pooling carries the small markets.** Each market alone at $10M dips below zero in 16% (USD-JPY)
  to 63% (UK 100) of paths, because its own fees are small next to its gap losses. Together, the
  crypto markets' fees (about $12M of the $28.7M) fund the others' risk.
- **The total floor does not bind.** At the chosen per-market floors, a total floor at 25% to 100%
  of their sum changes neither risk nor refusals.

The assumptions that move these numbers most, each at $10M on every market, on 1,000 paths (the
baseline's own seeds give NAV below zero 7.7% and house cash below zero 0.65%):

| Assumption | NAV<0 ever | Cash<0 | NAV<-$100k | Bad debt mean |
|---|---|---|---|---|
| Baseline | 7.7% | 0.65% | 1.2% | $766k |
| No farming (no hedged flow) | 67.5% | 17.6% | 43.0% | $413k |
| One client direction for all markets | 12.8% | 2.1% | 3.3% | $766k |
| Farmers hold 10% of their pairs through every closure | 12.4% | 5.0% | 6.0% | $7.7M |
| Volatility x2 | 11.4% | 1.0% | 5.6% | $2.4M |
| Markets bootstrapped independently | 3.6% | 0.30% | 0.40% | $767k |
| Farming share x2 | 0.31% | 0.67% | 0 | $1.06M |
| Flow x0.25 / x4 | 8.2% / 3.0% | 0.65% / 0.45% | 0 / 0.30% | $192k / $3.1M |

- **Farming is what makes the house solvent early.** Hedged flow pays about a third of the fees.
  Without it, NAV dips below zero in two thirds of paths and house cash in one in six.
- **Correlated client direction is among the largest market risks.** When clients lean the same way in
  every market, the house's net exposure adds up across markets instead of netting.
- **Pairs held through closures are a risk floors barely touch.** A pair adds no net exposure, so a
  cap limits it only through the size of its orders, while a gap past a leg's balance is bad debt:
  $7.7M over 90 days in this stress. Margin over closures, not floors, controls it.

Decisions for the owner:

1. **The measure and the target.** NAV below zero at any hour is strict: with no capital, every
   floor that lets the house trade risks an early dip. House cash below zero is what clients feel:
   the vault holding less than their balances. A depth tolerance, such as NAV never below -$100k,
   sits between them: $10M on every market gives about 1% (0.9% on 2,000 paths, 1.2% on 1,000).
2. **Whether to seed the house.** The early dip happens before fees accrue. Seed capital, not
   floors, removes it.
3. **Leverage on gap-prone markets.** Bad debt from gaps past 100:1 and 200:1 margin (oil, silver,
   indices) and 10:1 earnings gaps on shares is untouched by floors.
4. **#111's last acceptance criterion.** It puts the chosen values into the production genesis.
   This study recommends values but changes no config.

## The question

The house is the counterparty to every trade and starts with no capital (#15 §2). Each exposure
cap, per market and in total, is the larger of a share of house NAV and a fixed dollar floor (#45,
PR #99). At launch, NAV is near zero, so the floors are the whole of the house's exposure: they
decide how much unbacked risk the house carries before trader losses have built its capital, and
how many orders it refuses while it is small (#111).

This study estimates, for each market and in total, how the floors trade the probability of house
insolvency over the first 90 days against the share of opening notional refused for capacity, from
papertrade.xyz's on-chain trader behaviour and the launch markets' price history. It recommends
floors at three insolvency targets and reports the sensitivity to the main assumptions. The values
are not applied to `config/production-genesis.toml`.

## Insolvency measures

Each is a probability over 90 days of simulated trading, starting from a house with no capital.

| Measure | Event |
|---|---|
| NAV below zero, ever | House NAV (house cash less clients' unrealised PnL) is below zero at some hour-end. |
| House cash below zero | At some hour-end house cash is below zero: the vault holds less USDC than clients' balances, which bound from above what clients may withdraw on demand. This is NAV below the withdrawals owed, on a realised basis. |
| Queue | Under the modelled withdrawals, some withdrawal waits in the #49 first-in-first-out queue because the vault cannot pay it. |
| NAV below zero at day 90 | House NAV is below zero at the end. |
| NAV below -$100k, -$1M | The lowest NAV falls below that depth. |

The calibration uses the first two. Below 1% (20 of 2,000 paths), a probability is extrapolated
from an exponential fit to the worst 5% of paths' lowest NAV or lowest house cash, and never
reported below the rate the paths show; when the fit does not apply and no path fails, it is the
95% upper bound for zero events, 3 in the number of paths.

The cost side is the share of opening notional refused for capacity (directional flow and churn),
in the first week, when the floors carry the house, and over the 90 days.

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
49,109,718 (2026-10-21 05:42 UTC) from six public HyperEVM RPC endpoints, 1,255 chunks of 1,000
blocks, and block timestamps every 10,000 blocks. 25 randomly chosen chunks, fetched again from an
endpoint chosen at random, matched their stored log counts. The raw pull is 212 MB and gitignored.

The Exchange's source is not verified on the explorer, so `derive/papertrade.py` infers the event
layouts from the logs and checks them:

- every one of the 1,580,269 closes and 132,923 liquidations matches one of the 1,715,360 opens;
- the median ratio of a close's fee to its haircut PnL is 0.0200, the documented 2% win fee and
  LP-side loss fee, over the 1,374,753 closes with non-zero haircut PnL;
- BTC entry prices sit within 0.1% (median) of Binance's BTCUSDT close of the same hour;
- deposits ($351.2M) less withdrawals ($305.9M) give $45.3M, against the $45.36M of USDC the
  explorer showed the Exchange holding on 2026-10-21.

The contract's open-interest event is not used: its changes do not follow position flows, and
until block 48,217,087 it carried a fixed offset of about $5B a side.

Trading opened at 2026-10-10 14:00 UTC, so the data covers 10.6 days: 18,504 accounts and $713B
of notional. Both legs of a hedge are *hedged*: a position its account opens while holding at
least as much opposite notional in the same asset, with the open opposite positions it offsets,
and two accounts' opposite positions of the same notional that open and close within two blocks of
each other. The derived dataset, committed under `data/derived/`, is the flow profile the model
samples (`papertrade_flow.csv`: margin by kind, hold class, and leverage bin), the size profile
(`papertrade_sizes.csv`: by kind, the margin-weighted distribution of each position's notional at
each launch leverage limit, 10:1, 100:1, and 200:1, under either leverage mapping), the daily series
(`papertrade_daily.csv`), the holding-time table (`papertrade_holding.csv`), and the summary
(`papertrade_summary.json`). `results/report.md` tabulates them. The flow and size profiles count
closed positions; the 2,168 still open at the end, the longest holds among them, are left out.

What the data says about the brief's farming premise:

- Hedged positions are 41% of notional and 67% of exposure-time; within accounts, 60% of gross
  exposure-time is offset by the same account's opposite positions, and market-wide 87%.
- Holds are short: the median position lasts 64 seconds, but 71% of exposure-time comes from
  positions held over an hour and 9% from positions held over a day.
- Hedged flow pays $0.235 of haircut and fee on its winning legs per $1 lost on its losing legs.
  On this engine a winning leg pays a 10% profit fee and no haircut, so the same farming costs about
  $0.10 per $1 of loss, and the house takes less than half of what papertrade takes from it.
- Flow fell from $373M of margin on the first full day to $79M on the last.

### Launch markets

`config/production-genesis.toml` lists 37 markets. `fetch/markets.py` pulls, for each:

| Data | Source |
|---|---|
| Hourly OHLC, 2024-10-28 to 2026-10-12 (102 weeks) | Crypto: Binance spot klines (`data-api.binance.vision`). Everything else: Yahoo Finance chart API (`query1.finance.yahoo.com/v8/finance/chart`), which serves hourly bars for the last 730 days. |
| Daily closes from 2016 | FRED (St. Louis Fed) where an official series exists: `DEXUSEU`, `DEXUSUK`, `DEXJPUS`, `DEXUSAL`, `SP500`, `NASDAQ100`, `DJIA`, `NIKKEI225`, `DCOILWTICO`, `DCOILBRENTEU`. Otherwise Binance daily klines (from August 2017; SOL from August 2020) or Yahoo daily bars, dividend-adjusted for shares. |
| Benchmark rates | FRED `SOFR` (3.87%), `IUDSOIA` (SONIA, 3.73%), `ECBESTRVOLWGTTRMDMNRT` (ESTR, 2.44%), each on 2026-10-19 or the latest before; TONA 0.75% and AONIA 3.60% are assumptions. Until #33 sequences rates the engine counts a benchmark as zero; the model charges these. |
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
- For crypto, FX, and futures, a gap after a hole of 3 to 24 hours is set to zero unless the hole
  touches a US market holiday or early close, or the day either side: the futures series switches
  contract at the other holes (ES jumped 2.6% across one on 2024-12-20). Gaps over the daily
  break, weekends, holiday halts, and holidays are kept.
- DE40 and UK100 fold each weekday overnight gap into continuous movement, since their venue
  futures trade 20 or more hours a day, and keep weekend gaps, taken to the first bar's close
  because the FTSE series opens at the previous close.

`market_stats.csv` gives each market's volatility, the reopening gaps a year past half its first
margin tier (where a position at the maximum leverage is closed out) and past the whole tier (where
it loses more than its balance), and the largest hourly and daily moves, over the hourly window
and over the daily history from 2016. `results/report.md` shows it in full.

For the ten markets with an official FRED series, `market_stats.csv` also compares week-on-week
returns of the hourly data, at Friday's last bar, with the FRED series. Correlation is 0.978 for
US500, 0.981 for US100, 0.943 for US30, 0.937 for WTI, and 0.935 to 0.953 for the FX pairs, whose
FRED rates are the noon New York fixings. JP225 (0.803) and Brent (0.816) are compared with the
Tokyo close and the London Brent spot assessment. Volatility ratios are 0.94 to 1.13. The hourly window is a high-volatility one for metals and several shares (gold 26% a
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
  one position per market. A pair ends when either leg is closed out, the other leg closing with
  it, or at its hold.
- Positions held under an hour (churn: 87% of margin, 93% of positions) pay the profit fee on their
  expected gain over their hold, at the hour's Parkinson volatility. Their mean open net notional
  over the hour competes for cap room and takes the hour's move, and churners fund what they owe.

Every opening order, directional, paired, or churn, must fit under its cap. An order whose fill
alone would take the house past a cap is refused whatever flows around it, so each class keeps
only the share of its margin in orders small enough to fit under the market and total caps, from
papertrade's size profile at the market's leverage limit. At 10:1, orders up to $1M carry 87% of
directional margin; at 200:1, 40%, because large-margin positions carry most margin. With no room, as at zero NAV and a zero floor, nothing
opens. Of what fits, the side that reduces the house's net exposure fills in full and the crowded
side up to the cap. A pair leaves the house's net exposure unchanged, so once its legs fit, no cap
refuses it.

**Engine rules** (`model/accounting.py`, each against the engine function it mirrors; the tests
check them on hand cases from the engine's own tests):

- a 10% profit fee on realised gains, nothing on losses (`profit_fee`);
- financing at each 22:00 rollover on every open position's notional at the mark: longs pay
  benchmark + markup, shorts markup - benchmark, at the genesis markups and day bases
  (`daily_financing`);
- close-out when equity falls below 50% of margin, at the trigger in continuous trading or at the
  bar's open when the market gaps through it, only while the market is open (`closeout.rs`);
- negative balance protection: a loss beyond the account's balance is bad debt against the house
  (`settle_bad_debt`);
- USD settlement (#98): USD-JPY, Japan 225 (yen), Germany 40 (euros), and UK 100 (pounds) accrue
  PnL in their quote currency and convert it at the engine's own USD-JPY, EUR-USD, or GBP-USD
  price, from the same bootstrapped weeks, so their margin, close-outs, caps, and NAV are in USD;
- each cap the larger of its genesis NAV share (`house_cap_bps`, `house_total_cap_bps`) and its
  floor; an order that would take the house's net exposure past a cap is refused (`check_risk`).

**Sensitivities.** Thirteen variants start from the floors the house-cash measure takes at 1%,
every market at the floor where it stops refusing orders: no farming, twice the
farming share, farming that halves every 30 days with the mint rate, pairs held through closures,
volatility x1.5 and x2, leverage mapped relative to each limit, markets bootstrapped
independently, one client long share for every market, flow x0.25 and x4, exits before closures,
and history with its drift kept. In the closure variant, at the last open hour before each closure
of 3 hours or more, farmers open pairs at the maximum leverage worth 10% of the hedged margin the
market accepted over about the past week, in orders that fit under the cap, and close them at the
reopening, where a gap past a leg's balance is bad debt. Each variant runs with every floor scaled
together by x0.1, x0.3, and x1.

**MONKEY.** Losing trades mint MONKEY at a rate per dollar of realised loss (#36), but genesis
sets neither the rate nor the half-life, and MONKEY moves no house cash. The model reports the
loss basis it mints on. The incentive enters through the hedged flow, which the farming share
scales, and a half-life on it is a sensitivity.

**Vault and withdrawals.** Exit proceeds go to clients' free balances. New margin is drawn from
free balances first and deposited for the rest. Half of each hour's remaining exit proceeds is
withdrawn, and free balances leave at 5% a day, or 55% a day while NAV is below zero (a run on a
public log), each compounded hourly. A withdrawal the vault cannot pay joins the #49 queue and is
paid as deposits arrive.

**Calibration.** Each market alone, at its share of the flow, gives each measure as a function of
its floor, over 18 floors from $0 to $100M. A *risk level* `q` sets every market's floor to the
largest floor at which that market alone stays at or below `q`. That floor is capped where the
market stops refusing orders (under 1% refused over 90 days) and is zero when even a zero floor
exceeds `q`. The total floor is the sum. All markets together are then run at twelve levels, from
0 to 50% and the binding floors, and a target takes the highest level whose joint measure meets it,
with every lower level meeting it too. The sensitivity stage's baseline adds uniform floors, $1M,
$3M, and $10M on every market. At the level chosen for 1%, a second
run sets the total floor to 25% to 100% of the sum, where the total cap starts to bind.

## Results

`results/report.md` holds every table: the data summaries, each market alone, all markets
together at each risk level with the floors chosen per target, the total-floor sweep, and the
sensitivities. The two tables below are all markets together.

Floors set for NAV below zero at some hour (each market's floor at the largest of its own tested
floors that stays at or below the level when the market runs alone):

| Level | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 | Fees mean | Bad debt mean |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 0.01%, 0.03% | $316k | 1 | <0.01% | <0.01% | 0 | 85.8% | 41.0% | $16.9M | $16.5M | $326k |
| 0.1% to 1% | $562k | 1 | 0.05% | <0.01% | 0 | 83.2% | 38.7% | $17.6M | $17.3M | $342k |
| 3% | $1.29M | 4 | 1.05% | 0.10% | 0 | 78.6% | 35.6% | $18.7M | $18.3M | $361k |
| 10% | $2.63M | 8 | 0.25% | <0.01% | 0 | 70.6% | 33.3% | $19.2M | $18.8M | $375k |
| 20% | $5.29M | 10 | 4.30% | <0.01% | 0 | 61.2% | 29.4% | $20.4M | $19.9M | $397k |
| 30% | $8.91M | 13 | 5.95% | 0.08% | 0 | 52.6% | 26.0% | $21.3M | $20.8M | $417k |
| 50% | $143M | 19 | 7.40% | 0.61% | 0.75% | 1.8% | 0.7% | $28.8M | $28.4M | $766k |
| binding | $370M | 37 | 7.85% | 0.60% | 0.90% | 0.1% | 0.0% | $29.0M | $28.7M | $774k |

Floors set for house cash below zero at some hour:

| Level | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 |
|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 0.01%, 0.03% | $1.03M | 2 | 0.90% | 0.05% | 0 | 79.5% | 35.7% | $18.6M |
| 0.1% | $1.10M | 2 | 0.60% | <0.01% | 0 | 78.4% | 35.3% | $18.8M |
| 0.3% | $1.18M | 2 | 0.70% | <0.01% | 0 | 77.9% | 35.1% | $18.8M |
| 1% | $2.79M | 3 | 8.40% | <0.01% | 0 | 73.1% | 31.9% | $20.0M |
| 3% | $5.57M | 7 | 12.0% | 0.50% | 0 | 65.1% | 29.2% | $20.7M |
| 10% | $46.1M | 11 | 12.5% | 0.27% | 1.30% | 35.0% | 17.0% | $23.9M |
| 20% | $140M | 17 | 8.15% | 0.60% | 1.20% | 1.6% | 0.5% | $28.7M |
| 30% | $150M | 19 | 8.10% | 0.60% | 1.20% | 1.6% | 0.5% | $28.7M |
| 50% | $340M | 34 | 7.95% | 0.60% | 0.90% | 0.5% | 0.1% | $29.0M |
| binding | $370M | 37 | 7.85% | 0.60% | 0.90% | 0.1% | 0.0% | $29.0M |

"<0.01%" is an extrapolation from the tail, below what 2,000 paths can count. Refused shares are
of opening notional, directional flow and churn.

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
  triggered by financing (financing is charged but does not move a position's trigger), the
  stale-price rule that stops dealing when venues disagree (#15 §1), and its #98 form for a
  market whose FX market has no price.
- **Order sequencing and size.** Within an hour the model nets what fits: it assumes the orders
  that reduce exposure arrive spread among the crowded ones. When room is small next to the orders,
  the engine, checking them one by one, refuses more. Orders keep papertrade's sizes; a trader who
  splits an order under a small cap gets more of it filled than the model gives.
- **Close-out fills.** In continuous trading a close-out fills at its trigger. A crash inside an
  hour that the venue index jumps across fills worse in the engine, so crypto bad debt is
  understated.
- **Withdrawals.** The withdrawal and run rates are assumptions; the queue measure depends on
  them, and the house-cash measure does not.

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
- `tests/`: the engine rules, the simulation's accounting, the data derivation, and the
  estimators, on hand cases and small synthetic inputs.
- `results/`: the stage outputs, `report.md`, and run manifests.
