# engine-core

## Overview

The deterministic state machine at the heart of the house. It applies sequenced commands (orders,
venue prices, deposits, withdrawals, operator actions, and time ticks) and returns events. It
does no IO and uses no clocks, randomness, or floats, so anyone can replay the public log against
this crate and reproduce the signed state roots exactly.

## Features

- House as counterparty to every trade, with per-market and total exposure caps against house NAV,
  from protocol `V6` with a fixed dollar floor under each so trading opens from a zero or negative
  house
- Instant fills at the median of several venues, refused when quotes are stale or divergent
- Zero spread at normal size, fixed-bps size spreads on position growth, widened under stress
- Indicative quotes with bounded slippage: orders are declined only when the bound is exceeded
- A flat fee on realised profit only, and daily CFD financing at a benchmark rate plus markup
- USD accounts: markets quoted in euros, pounds, or yen accrue PnL in that currency and convert it to
  USD at the engine's own index for the FX market, and have no price while that FX market has none
- Tiered margin, account close-out largest loser first among markets that can trade, and negative
  balance protection; from protocol `V6`, close-outs also follow financing and parameter or listing
  changes, skip a market outside its session, and value a market that can trade at its live index
- Venue trading hours for RWA markets, with holidays, early closes, and special sessions, and
  resting orders filling at the gapped open
- Markout-based toxicity scoring that widens spreads for flow that picks off stale prices
- Deposits credited at most once per chain reference, and all external numbers bounded
- A keccak-256 state root: from protocol `V9`, the root of a sparse Merkle tree over accounts,
  markets, and the rest of the state, which each root rehashes only where commands changed it;
  before, the hash of the canonical state encoding
- Proofs of an account's balance and positions, and of its markets' marks, against a root, laid out
  for a Solidity verifier (keccak-256, 32-byte nodes, explicit left and right) and checked without
  a node
- Versioned rules: a log activates each protocol version at a recorded seq, and a log that never
  does replays under `V2`, the rules every log starts under, to its original roots
- From protocol `V3`, client auth recorded in the log: EIP-712 signatures, per-account nonces, and
  expiries on every order, cancel, and withdrawal, checked by the engine so a replay proves each
  request was its account's, or unsigned requests in dev logs
- From protocol `V5`, position accounting rounds in the house's favour: an averaged entry is the
  exact average rounded once, up for a long and down for a short, realised and unrealised PnL round
  down, and the profit fee and financing charges round up, so no sequence of fills realises more
  than its exact PnL
- From protocol `V8`, admin commands signed by the operator key the log records, with the operator's
  nonces and expiries, limited to listings, parameters, benchmark rates, stress floors, and
  reduce-only; once any deposit or house capital is credited, an unsigned one is refused, so only
  genesis runs unsigned
- One command that makes every market reduce-only, including markets listed later; trades that
  reduce or close a position fill in every reduce-only mode, and from `V8` parameters whose stress
  and toxicity spreads would reach the price are refused
- From protocol `V10`, spreads and margin round in the house's favour: every spread charge rounds up
  for the trader paying it, buying or selling, though a sell's stops a unit short of the price, and
  every margin requirement rounds up, from a position's notional to the close-out level
- Products and quotients that never overflow while the result fits, whatever the position size or
  price ratio

## Repository Structure

- `src/lib.rs` — module index and the protocol summary
- `src/engine.rs` — engine state, command dispatch, sessions, prices, markouts, and USD valuation
- `src/execution.rs` — order placement, triggers, pricing, risk checks, and booking fills
- `src/closeout.rs` — account close-out and bad-debt write-off
- `src/funds.rs` — deposits, withdrawals, house capital, and overnight financing
- `src/admin.rs` — operator commands, signed admin commands, and stress levels
- `src/auth.rs` — client auth, the operator record, request signatures and nonces, and the checks
  every client request and signed admin command pass
- `src/eip712.rs` — the EIP-712 domain, the request types clients and the operator sign, and signer
  recovery
- `src/protocol.rs` — protocol versions, and how to add one
- `src/merkle/mod.rs` — the state tree's layout and the sparse Merkle tree
- `src/merkle/leaf.rs` — each leaf's key and value hash, and the account and market leaves
- `src/merkle/proof.rs` — account and market proofs, and how to check them
- `src/merkle/state.rs` — the engine's state tree: what each root rehashes, the remaining-state
  leaf, and account proofs
- `src/merkle/tests.rs` — tree, proof, and work tests, and the layout fixture
- `src/tracked.rs` — maps and sets that record what changed since the last root
- `src/fixed/` — `Fixed`, the 9-decimal fixed-point type used for all money (`mod.rs`), and its
  unit tests (`tests.rs`)
- `src/market.rs` — market listings, tier tables, and quote currencies with their USD conversion
- `src/params.rs` — protocol parameters, stress levels, toxicity, and the rollover clock
- `src/price.rs` — venue quotes and the reference index
- `src/session.rs` — weekly trading windows, holidays, dated closures such as early closes, and
  special sessions
- `src/account.rs` — accounts, netted positions, and resting orders
- `src/command.rs` / `src/event.rs` — engine inputs and outputs
- `src/types.rs` — ids, sides, and asset classes
- `tests/` — protocol tests through `Engine::apply`, with a shared harness, its signing client
  (`client.rs`), and the reference-log format in `tests/common/`; `tests/operator.rs` covers signed
  admin commands and every-market reduce-only
- `tests/reference/` — one reference log per protocol version: each line is a sequenced command and
  the state root after it
- `tests/reference_logs.rs` — replays every reference log and fails at the first seq whose root
  differs
- `tests/invariants/` — property tests over generated sessions (`main.rs`), the session generator
  (`session.rs`), fills at one price (`rounding.rs`), and spreads and margin against their exact
  amounts (`charges.rs`)
- `tests/invariants/state_root.rs` — incremental roots against a rebuild, with every account's
  proof checked, under generated sessions
- `tests/state_root.rs` — the state root from `V9`: its activation, what each root rehashes,
  account proofs, and roots against a rebuild
- `tests/fixtures/eip712.json` — EIP-712 test vectors: API bodies, wallet typed data, and
  signatures
- `tests/fixtures/merkle.json` — state tree vectors computed with Foundry's `cast`: keys, leaf ABI
  encodings and value hashes, a root, and proofs
- `examples/reference_log/` — writes the reference log for a new protocol version: the session
  (`main.rs`), the requests and event names it uses (`helpers.rs`), and its `V10` part (`v10.rs`)
- `examples/sign_request.rs` — a client signing a request body for the API
- `examples/verify_proof.rs` — checks an account proof from the API against a root

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

From `V9`, `Engine::account_proof` proves an account's leaf, and the leaf of each market it holds
a position in, against the last root computed, and `AccountProof::verify` checks it against a root
from a signed checkpoint. `src/merkle/mod.rs` and `src/merkle/leaf.rs` document the layout a
Solidity verifier follows. To check a proof the node serves:

```bash
curl -s http://127.0.0.1:8080/v1/accounts/<address>/proof |
    cargo run -p engine-core --example verify_proof -- <root>
```

A client signs each request with its account key under the EIP-712 domain of the vault, with the
account's next nonce and an expiry at most five minutes ahead:

```bash
SIGNER_KEY=<private key> cargo run -p engine-core --example sign_request -- \
    <chain id> <vault> <nonce> <expires at> <order|cancel|withdrawal> '<unsigned body>'
```

The operator signs each admin command the same way, as its JSON under the vault's domain, with the
operator's next nonce; `engine admin`, in `engine-node`, signs and submits it.

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
- `cargo test -p engine-core --test invariants` runs 1,024 generated sessions from a fixed seed, each
  under `V2`, `V6`, `V7` with markets quoted in other currencies, and `V10`. Each mixes client, feed, custody, and operator commands, with
  values inside and outside the input bounds, and checks after every command that nothing panicked,
  cash is conserved, and every market's aggregates match its accounts' positions. Raise `CASES` or
  change `SEED` in `tests/invariants/main.rs` to search further. The same suite generates adds,
  reductions, and flips at one price under `V5` and checks that none realises a profit, and fills
  in random spread and margin bands under `V10` and checks that none pays less spread or posts
  less margin than the exact amount.
  Under `V9`, the same sessions check that roots computed from what changed, at random points,
  equal a root hashed from scratch, and that every account's proof verifies against them.

### Adding a protocol version

A change to engine state, its serialization, or what a command does changes the roots that existing
logs replay to. It ships as a new protocol version, and the committed logs of earlier versions must
keep passing unchanged. `v1.jsonl` holds the original rules, and `v2.jsonl` adds two: a venue quote
stamped more than `MAX_QUOTE_LEAD_MS` after the sequenced time is refused, and any quote replaces a
stored one stamped after the sequenced time. Both predate activation, so every log starts under
their combined rules, which `src/protocol.rs` names `V2`; `v1.jsonl` still replays because none of
its quotes is stamped ahead. `V3` adds signed client requests and activates at a recorded seq; its
log is `v3.jsonl`. `V4` lets schedules carry dated closures and special sessions, and lets a zero
house capital move record its reference; it activates the same way, and its log is `v4.jsonl`. `V5`
rounds position accounting in the house's favour; its log is `v5.jsonl`. `V6` adds dollar floors
under the house exposure caps, close-out checks after financing and parameter or listing changes,
and a session check and live-index valuation on every close-out; its log is `v6.jsonl`. `V7` adds
markets quoted in another currency than USD (`MarketSpec::quote_currency`): their PnL, margin, house
exposure, and financing convert to USD at an FX market's index, rounding toward the house on top of
`V5`'s rounding, and they have no price while that FX market has none. Before `V7` activates, a
listing that declares another quote currency is refused, and a USD market has no FX mark and encodes
as before, so earlier logs replay unchanged; its log is `v7.jsonl`. `V8` adds admin commands signed
by the operator, refuses an unsigned operator command once anything is credited, adds every-market
reduce-only, and refuses parameters whose spreads would refuse a close; it activates the same way,
and its log is `v8.jsonl`. `V9` makes the state root the root of the state tree; its log is
`v9.jsonl`. `V10` rounds spreads and margin in the house's favour; its log is `v10.jsonl`. For each
later version:

1. Add the version in code as `src/protocol.rs` lists, then extend the session in
   `examples/reference_log/main.rs` so it activates the new version and exercises the new rules.
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
