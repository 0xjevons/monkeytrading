# Close-spread calibration

This study supports #147 and the per-market operator ceilings in #112. No production ceiling
has been selected. A price move is market risk, not an execution spread charged by the house.

The pipeline pins the 37 launch markets and audits the existing #111 inputs. Its outputs
are preliminary evidence, with a separate calibration and validation sample. An unsupported
market has a null recommendation; it never inherits another market's ceiling.

## Reproduce

Use Python 3.12 or newer and the exact dependencies in `requirements.txt`, in
a virtual environment outside the repository. From the repository root:

```sh
python -I research/close-spread-calibration/run.py
python -I research/close-spread-calibration/report.py
python -I -m unittest discover -s research/close-spread-calibration/tests
cargo test -p engine-core --test close_spread_calibration --locked
```

The pipeline performs no network requests. It hashes its inputs, verifies the pinned genesis,
and writes JSON under `results/`. The historical dataset has been transformed for another
study; its statistics are not sufficient to select close-spread ceilings.

The paired-account sweep also writes deterministically compressed CSV. It starts with two already-admitted opposite
positions in separate accounts, jumps directly to one new price, liquidates an account whose
index equity is below the configured close-out threshold, and voluntarily closes the other.
Liquidation waives toxicity. Negative balances are written off; the configured profit fee
applies only to positive realised profit. Reported exit debits are nominal execution costs;
negative-balance protection can write part of a debit off, which the separate bad-debt and
house-cash columns reflect. This is a conditional isolated-position sensitivity,
not an admission/cross-margin simulator or a measured probability of house failure.

The explicit candidate caps are 0, 1, 2, 3, 5, 10, 13, 25, 50 and 100 bps. Stress minimums are
0 and 10 bps (configured launch values) and a hypothetical 100 bps operator change. Toxicity is
3 bps when not liquidating. None of these scenario values is a proposed production ceiling.
Capital scenarios are 0%, 1% and 10% of paired gross notional. Position sizes are midpoints of
bounded margin bands and twice the last bound for the final band. Positions may exceed launch
capacity: the sweep conditions on existing exposure rather than asserting those positions can
be opened. Shocks include flat, ±1%, ±5% and both directions of the inherited calibration and
validation hourly/gap maximum magnitudes. Turning an absolute magnitude in both directions is
a stress assumption, not a claim that both events were observed. Reopening gaps are not charged
as spreads. No historical test-set observation selects a candidate or recommendation.

Weekly block-bootstrap intervals use 200 deterministic resamples with seed 147, separately for
the calibration and validation samples. They describe sampling sensitivity under weekly
stationarity; they do not cover missing feeds, proxy errors or unseen tail events.

## Engine model boundary

At engine revision `75e11bc3082adbebc8b74140e0697cf86eecc00e`, a pure reduction has no size
spread. It pays the stress minimum plus the account's toxicity add-on; liquidation waives the
toxicity add-on. A reversal also pays size spread on its opening slice, distributed over the
whole fill quantity. The size-spread stress multiplier applies to that slice only.

Eleven shared vectors compare the research price model with actual `Engine::apply` fills,
including a liquidation gap that creates bad debt. They are research fixtures, separate from
the immutable protocol reference logs. Charges round upward at nine decimal places. A sell's adjustment stops one raw unit below its
index price. Cross-currency conversion introduces additional rounding. The integer spread
reported in a Fill is truncated basis points; checking only that integer against a proposed
ceiling can miss a fractional-basis-point excess. A #112 ceiling should bound the actual price
adjustment, with an explicit raw-unit rounding rule, and must distinguish a reversal's opening
slice from the protected closing quantity. That reversal rule remains a proposal for review.

The current configuration's maximum pure-close nominal charge is 13 bps (10 stress plus 3
toxicity); liquidation's is 10 bps, before raw-unit effects. These are configured charges, not
calibrated ceilings or recommendations. The operator can change stress/toxicity parameters,
which is why a separate immutable ceiling matters. Volatility does not automatically change
that immutable ceiling.

## Evidence limitations

- The inherited hourly grid covers 2024-10-28 to 2026-10-12 UTC. The raw fetch extends further;
  the grid's 102 whole weeks are authoritative for this pipeline.
- #111 zeroes selected inter-bar and presumed roll gaps and folds cash-index overnight moves.
  DE40 and UK100 are cash-index proxies for longer-session futures. These transformations cannot
  establish actual reopening gaps, contract-roll effects, or execution-horizon risks.
- Crypto uses Binance USDT spot rather than a multi-venue USD reference. The inherited dataset
  has no simultaneous independent feeds, quote ages, or tick/second bars. It cannot measure
  execution-scale staleness or divergence, and USDT/USD basis is unmeasured.
- Yahoo front-month futures lack explicit contract identifiers and roll/corporate-action audit
  trails here. The raw data is not committed. Its absence must not become an implicit clean bill
  of health. Public availability does not establish redistribution rights.
- Historical holdings/flow from another venue are scenario inputs, not representative launch
  behaviour. Do not import its loss-token farming assumptions as approved product behaviour.

## Retained observation audits

`observations.py MANIFEST.json BODY.bin` audits one retained response without network access.
Its manifest records the exact HTTPS URL, UTC retrieval start/end, status, complete/truncated
flags, byte count, SHA-256, origin and explicit provider format. A self-consistent manifest is
not independent proof of source identity. Coinbase trade and Kraken level timestamps never
populate client arrival times. Dukascopy additionally requires an explicit verified price scale,
UTC hour anchor and decompression limits. Incomplete inputs produce no observations and exit 2;
no forward fill, gap suppression or partial-batch salvage occurs. Original raw captures and
acquisition scripts remain in the ignored evidence area pending permitted-use decisions.

## Remaining delivery

Audit raw source access, licensing, session gaps, corporate actions and actual reference
contracts; obtain execution-relevant independent observations where available. Extend the
verified pricing vectors and conditional isolated-position sweep to complete correlated
portfolio/cross-margin and house-capacity scenarios. Report trader cost and house shortfall
separately. Candidate grids and risk tolerances are
explicit scenario assumptions; selecting a production risk tolerance requires owner adoption.
The preliminary report includes sampling uncertainty, separate held-out observations and plots;
these do not replace missing real-source evidence. An end-to-end closing/liquidation risk
verification scenario and owner-adopted per-market values remain necessary before #147 is
complete. No protocol versions, protocol fixtures or production values change.
