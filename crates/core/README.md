# engine-core

## Overview

The deterministic state machine at the heart of the house. It applies sequenced commands (orders,
venue prices, deposits, withdrawals, operator actions, and time ticks) and returns events. It
does no IO and uses no clocks, randomness, or floats, so anyone can replay the public log against
this crate and reproduce the signed state roots exactly.

## Features

- House as counterparty to every trade, with per-market and total exposure caps against house NAV
- Instant fills at the median of several venues, refused when quotes are stale or divergent
- Zero spread at normal size, fixed-bps size spreads on position growth, widened under stress
- Indicative quotes with bounded slippage: orders are declined only when the bound is exceeded
- A flat fee on realised profit only, and daily CFD financing at a benchmark rate plus markup
- Tiered margin, account close-out largest loser first, and negative balance protection
- Venue trading hours for RWA markets, with resting orders filling at the gapped open
- Markout-based toxicity scoring that widens spreads for flow that picks off stale prices
- Deposits credited at most once per chain reference, and all external numbers bounded
- A keccak-256 state root over the canonical state encoding

## Repository Structure

- `src/lib.rs` — module index and the protocol summary
- `src/engine.rs` — engine state, command dispatch, sessions, prices, and markouts
- `src/execution.rs` — order placement, triggers, pricing, risk checks, and booking fills
- `src/closeout.rs` — account close-out and bad-debt write-off
- `src/funds.rs` — deposits, withdrawals, house capital, and overnight financing
- `src/admin.rs` — operator commands and stress levels
- `src/fixed.rs` — `Fixed`, the 9-decimal fixed-point type used for all money
- `src/market.rs` — market listings and tier tables
- `src/params.rs` — protocol parameters, stress levels, toxicity, and the rollover clock
- `src/price.rs` — venue quotes and the reference index
- `src/session.rs` — weekly trading windows and holidays
- `src/account.rs` — accounts, netted positions, and resting orders
- `src/command.rs` / `src/event.rs` — engine inputs and outputs
- `src/types.rs` — ids, sides, and asset classes
- `tests/` — protocol tests through `Engine::apply`, with a shared harness in `tests/common/`

## Usage

```rust
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

let mut engine = Engine::new();
let events = engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick })?;
let root = engine.state_root();
```

The node crate (`engine-node`) stamps and logs every command before applying it; a verifier
replays that log with the same calls.

## Testing

```bash
cargo test -p engine-core
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
