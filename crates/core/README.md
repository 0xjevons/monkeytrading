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
- Venue trading hours for RWA markets, with holidays, early closes, and special sessions, and
  resting orders filling at the gapped open
- Markout-based toxicity scoring that widens spreads for flow that picks off stale prices
- Deposits credited at most once per chain reference, and all external numbers bounded
- A keccak-256 state root over the canonical state encoding
- Versioned rules: a log activates each protocol version at a recorded seq, and a log that never
  does replays under `V2`, the rules every log starts under, to its original roots
- From protocol `V3`, client auth recorded in the log: EIP-712 signatures, per-account nonces, and
  expiries on every order, cancel, and withdrawal, checked by the engine so a replay proves each
  request was its account's, or unsigned requests in dev logs

## Repository Structure

- `src/lib.rs` — module index and the protocol summary
- `src/engine.rs` — engine state, command dispatch, sessions, prices, and markouts
- `src/execution.rs` — order placement, triggers, pricing, risk checks, and booking fills
- `src/closeout.rs` — account close-out and bad-debt write-off
- `src/funds.rs` — deposits, withdrawals, house capital, and overnight financing
- `src/admin.rs` — operator commands and stress levels
- `src/auth.rs` — client auth, request signatures and nonces, and the check every client request
  passes
- `src/eip712.rs` — the EIP-712 domain, the request types clients sign, and signer recovery
- `src/protocol.rs` — protocol versions, and how to add one
- `src/fixed.rs` — `Fixed`, the 9-decimal fixed-point type used for all money
- `src/market.rs` — market listings and tier tables
- `src/params.rs` — protocol parameters, stress levels, toxicity, and the rollover clock
- `src/price.rs` — venue quotes and the reference index
- `src/session.rs` — weekly trading windows, holidays, dated closures such as early closes, and
  special sessions
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
- `tests/fixtures/eip712.json` — EIP-712 test vectors: API bodies, wallet typed data, and
  signatures
- `examples/reference_log.rs` — writes the reference log for a new protocol version
- `examples/sign_request.rs` — a client signing a request body for the API

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

A client signs each request with its account key under the EIP-712 domain of the vault, with the
account's next nonce and an expiry at most five minutes ahead:

```bash
SIGNER_KEY=<private key> cargo run -p engine-core --example sign_request -- \
    <chain id> <vault> <nonce> <expires at> <order|cancel|withdrawal> '<unsigned body>'
```

## Configuration

`SIGNER_KEY`: the account's private key, hex, for the `sign_request` example.

## Testing

```bash
cargo test -p engine-core
```

Two suites guard replay and accounting:

- `cargo test -p engine-core --test reference_logs` replays every log in `tests/reference/` and fails
  at the first seq whose state root differs from the committed one. CI runs it on x86_64 and
  aarch64 Linux, and `scripts/check-reference-logs.sh` fails a branch that changes, renames, or
  deletes a committed log.
- `cargo test -p engine-core --test invariants` runs 1,024 generated sessions from a fixed seed. Each
  mixes client, feed, custody, and operator commands, with values inside and outside the input
  bounds, and checks after every command that nothing panicked, cash is conserved, and every
  market's aggregates match its accounts' positions. Raise `CASES` or change `SEED` in
  `tests/invariants/main.rs` to search further.

### Adding a protocol version

A change to engine state, its serialization, or what a command does changes the roots that existing
logs replay to. It ships as a new protocol version, and the committed logs of earlier versions must
keep passing unchanged. `v1.jsonl` holds the original rules, and `v2.jsonl` adds two: a venue quote
stamped more than `MAX_QUOTE_LEAD_MS` after the sequenced time is refused, and any quote replaces a
stored one stamped after the sequenced time. Both predate activation, so every log starts under
their combined rules, which `src/protocol.rs` names `V2`; `v1.jsonl` still replays because none of
its quotes is stamped ahead. `V3` adds signed client requests and activates at a recorded seq; its
log is `v3.jsonl`. `V4` lets schedules carry dated closures and special sessions, and lets a zero
house capital move record its reference; it activates the same way, and its log is `v4.jsonl`. For
each later version:

1. Add the version in code as `src/protocol.rs` lists, then extend the session in
   `examples/reference_log.rs` so it activates the new version and exercises the new rules.
2. Write the version's log, which the generator refuses to do over an existing file:
   `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`
3. Write the node's log from it, with the events each command emits:
   `cargo run -p engine-node --example reference_log -- crates/core/tests/reference/v<N>.jsonl
   crates/node/tests/reference/v<N>.jsonl`
4. Commit both files. The replay tests and CI pick them up with no other change.

The node's logs carry each command's events, and its replay refuses events that differ from what
the engine emits. A change to the events a command emits, or to how they serialize, is therefore a
protocol change too, even when state roots stay the same.

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
