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
- `tests/` — protocol tests through `Engine::apply`, with a shared harness and the reference-log
  format in `tests/common/`
- `tests/reference/` — one reference log per protocol version: each line is a sequenced command and
  the state root after it
- `tests/reference_logs.rs` — replays every reference log and fails at the first seq whose root
  differs
- `tests/invariants/` — property tests over generated sessions (`main.rs`) and the session generator
  (`session.rs`)
- `examples/reference_log.rs` — writes the reference log for a new protocol version

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

Two suites guard replay and accounting:

- `cargo test -p engine-core --test reference_logs` replays every log in `tests/reference/` and fails
  at the first seq whose state root differs from the committed one. CI runs it on x86_64 Linux,
  aarch64 Linux, and macOS, and `scripts/check-reference-logs.sh` fails a branch that changes,
  renames, or deletes a committed log.
- `cargo test -p engine-core --test invariants` runs 1,024 generated sessions from a fixed seed. Each
  mixes client, feed, custody, and operator commands, with values inside and outside the input
  bounds, and checks after every command that nothing panicked, cash is conserved, and every
  market's aggregates match its accounts' positions. Raise `CASES` or change `SEED` in
  `tests/invariants/main.rs` to search further.

### Adding a protocol version

A change to engine state, its serialization, or what a command does changes the roots that existing
logs replay to. It ships as a new protocol version, and the committed logs of earlier versions must
keep passing unchanged. The engine has one protocol version today, `v1.jsonl`; versions that
activate at a recorded seq arrive with signed client requests (#18). For each later version:

1. Extend the session in `examples/reference_log.rs` so it activates the new version and exercises
   the new rules.
2. Write the version's log, which the generator refuses to do over an existing file:
   `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`
3. Commit the file. The replay test and CI pick it up with no other change.

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
