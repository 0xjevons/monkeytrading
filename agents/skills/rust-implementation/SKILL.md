---
name: rust-implementation
description: Use when writing, editing, or reviewing Rust in this repo (the resulting implementation). Covers edition and toolchain, safety, error handling, input-handling doctrine, determinism of engine-core, numeric types and money, iterators, async, process-global state, configuration, serde, logging, dependencies, workspace layout, file structure, file line cap and module splits, crate README, and Cargo profiles.
---

Load when implementing or modifying Rust code, or reviewing a Rust change.

## Vendored reference

For detailed Rust patterns, consult the vendored upstream pack:

- Path: `../vendor/rust-skills/skills/`
- Source: `https://github.com/actionbook/rust-skills`
- If the pack is absent, run `../vendor/pull.sh rust-skills` before loading vendored Rust files.
- If a routed vendored Rust file is missing, run the pull, retry the exact read, and stop with the
  missing path if it still fails; do not continue with only this wrapper's guidance.
- Start at `rust-router/SKILL.md` when the relevant sub-skill is not obvious; load only the triggered sub-skill's `SKILL.md`, not the whole tree.
- The repo conventions below and the rules win when they conflict with vendored guidance.

## General

- Use Rust 2024 edition and the latest stable toolchain; no nightly features unless absolutely necessary and approved.
- Keep dependencies current and 2024-compatible; verify the latest version on crates.io when adding one.
- Keep code simple and idiomatic — use 2024 features freely without overcomplicating.
- Avoid `unsafe`. If unavoidable, ask first and document both the function and the reasoning.
- Minimize bugs by leveraging the type system; share common types across crates through `engine-core`. Prefer clear domain types over stringly-typed ids, states, paths, addresses, and names (`AccountId`, `MarketId`, `Side`, `Fixed`).
- Exhaust other options (generics, associated types, refactoring) before macros. Ask before writing a macro, and document it thoroughly.

## Iterators and loops

- Prefer iterator / functional style (`map`, `filter`, `fold`); consider `itertools` before complex chains.
- Use loops only when they read better, and avoid nested loops.

## Error handling

- Executables use `anyhow`; libraries use `thiserror`. Reserve panics for the truly unrecoverable.
- Do not `unwrap()` for panic cases. Prefer proper handling; if unavoidable, use `expect()` with a clear message.
- In `engine-core`, a rejected command is an event (`OrderRejected`, `WithdrawalRejected`,
  `AdminRejected`) with a typed reason, never an `Err` or a panic: the rejection is part of the
  public log.

## Input-handling doctrine

The engine consumes commands from clients, price feeds, custody, and the operator. Classify every
failed check on such input:

- **Detected error or inconsistent/confusing state in input data → never act on it.**
  Reject, log, and return without mutating state — leave everything as-is. Do not "recover" by
  guessing a price, a balance, or an intent from data just proven untrustworthy. A stale or
  divergent feed means no price, not the last price.
- **Internal invariants — states our own code cannot produce without a bug → hard
  `assert!` / `expect` and crash.** Add a terse comment on why the state is
  (almost) impossible. Never `debug_assert!`.
- Before softening any assert, trace the trigger: client, venue, and chain content, and
  delivery/replay effects, are real; content the sequencer cannot produce is not.
- The same check applied to multiple inputs lives in one shared helper, not copies.
- An inconsistent log found at recovery or replay (a gap, a reversed timestamp, a root mismatch)
  is input, not an internal invariant: refuse to serve and report it for an operator, without
  writing anything — the sequencer must never append to a log it cannot verify.
- Ledger integrity beats uptime: anything that would let trading continue on possibly-diverged
  books — cash not conserved between accounts and the house, positions not matching the per-market
  aggregates — fails closed (crash).

## Determinism (`engine-core`)

`engine-core` is replayed by third parties against the public log, so the same commands must give
the same state root on every machine and build.

- No clocks (`SystemTime`, `Instant`), randomness, floats, threads, or IO. Time is the sequenced
  `ts`.
- Collections are `BTreeMap` / `BTreeSet` / `Vec`; never iterate a `HashMap` or `HashSet`.
- Every input a decision depends on arrives in a `Command`. Anything the node computes from
  outside data (an index, a rate) is either recomputed by the engine from logged inputs, or logged
  itself.
- The state root hashes the canonical serialization of the whole `Engine`; a field added,
  renamed, or reordered changes every root after it. Treat that as a protocol change
  ([development-checklist](../../rules/development-checklist.md), Determinism and replay).
- Pure helpers stay pure: no `pub` function in `engine-core` takes `&mut` access to anything but
  the engine it is called on.

## Numeric types and money

- `engine_core::Fixed` (an `i128` scaled by 10^9) is the type for every amount, price, quantity,
  and rate in engine state. `mul` / `div` truncate toward zero; release builds keep
  `overflow-checks = true`, so overflow halts rather than wraps.
- Never use `f32` / `f64` for money, prices, or anything that reaches engine state. A float is
  acceptable only outside the determinism boundary for display or mock data, converted to `Fixed`
  before it becomes a command.
- Basis points are `i64` applied with `Fixed::mul_bps` / `Fixed::from_bps`.
- On-chain amounts stay integer base units (`U256` or `u128`) until the custody boundary, where
  they convert to `Fixed` with the token's decimals; that conversion is checked and rejects
  precision loss.
- `Fixed` serializes as a decimal string so no JSON client loses precision; keep it that way at
  every API surface.

## Concurrency and async

- Prefer channel-based communication (`tokio::sync::mpsc` or similar) over shared state unless asked otherwise or clearly justified. The sequencer owns the engine; readers send it a query.
- Keep cancellation, polling, and timeout behavior explicit; a wait must never hang forever. Remote, process, and network calls carry explicit timeouts / deadlines, with timeout errors propagated with enough context to identify the target and operation.
- Long-running work must respect cancellation: dropping the parent future stops child work, spawned processes are killed or joined, and background tasks have an explicit shutdown path.
- A spawned task that the node cannot run without (sequencer, API server) is awaited by `main`, so its exit stops the process.
- Parallelize expensive independent work when safe (e.g. read-only ops across targets), preserving deterministic output ordering; keep ordered mutation paths sequential when state-transition safety depends on one-at-a-time behavior.

## Process-global state

No process-global mutable state. A `static` that holds runtime state (including via `LazyLock` or
`OnceLock`) is shared by every test in the binary: a test cannot get a fresh instance or reset it,
and can only assert before/after deltas that any concurrently running test can move. It also hides
a dependency that no constructor shows and limits the process to one instance.

- **Build it in `main` and pass it in.** Metrics, counters, caches, registries, config, clients,
  keys, and channels are created at startup and handed to the components that use them, as
  `main` does with the enclave key, the stream sender, and the sequencer channel.
- **Metrics register into a registry the node owns**, never a library's default global registry.
- **Allowed statics:** the `#[global_allocator]`; immutable values that depend on no config,
  environment, or runtime input (constant tables, compiled regexes, a monotonic clock epoch); an
  atomic counter that only mints unique ids and is never read back or asserted on; and test-only
  infrastructure behind `#[cfg(test)]` or a test feature.

## Configuration

The node takes one root `NodeConfig` from a TOML file, loaded and validated once at startup by
`NodeConfig::load`. The file path is the only CLI surface (`--config`); everything else is a
config key.

- **Optionality belongs to the section, not the field.** An optional process or feature is
  `Option<Section>`, and every field inside it is plain and required — presence of the section
  means it is configured. Never make each field `Option` and then hand-write a list of missing
  config paths: serde already names the missing field.
- **`#[serde(deny_unknown_fields)]` on node config section structs**, so a typo fails the boot
  instead of silently keeping a default. Protocol types in `engine-core` (`MarketSpec`,
  `ProtocolParams`) also travel in the log; whether they reject unknown fields is a log
  compatibility decision, made explicitly.
- **Nothing credential-bearing is a plain `String`.** Wrap secrets and credential-bearing URLs in
  a redacting newtype whose `Debug` and `Display` hide the value, and read it only at the point of
  use. Add the type with the first secret the node needs.
- **Keep validation structural.** Ranges, non-empty tables, and parseability belong in the type's
  `validate`; assume an operator-supplied config is intentional. Guard a value whose wrong setting
  fails **silently** — a zero window that can never trip, a tier table out of order.
- **Classify every new constant.** Before adding one, decide whether it is a true invariant or an
  operational preference. Keep invariants in code; put adjustable protocol values in
  `ProtocolParams` or `MarketSpec` (so changes are sequenced and logged), and node-only
  preferences in `NodeConfig`.
- **Tests must not mutate the process environment.** `set_var`/`remove_var` are `unsafe` in Rust
  2024 and race every concurrently running test in the binary. Take configuration as an explicit
  value instead, and assert on a committed example file.
- Build shared clients once — RPC provider, HTTP client — in `main`, and pass them to the
  components that need them rather than letting each build its own.
- CLI flags have both a short and long form where clap allows.
- For conditional compilation, use the attribute macro rather than the declarative one unless not viable.

## Serialization

- Maximize the serde ecosystem. Implement `Serialize` / `Deserialize` for a format instead of a bespoke interface.
- Commands, events, and log entries are internally tagged (`#[serde(tag = "type")]`) with
  `snake_case` names; keep new variants consistent so the public log stays readable.

## Logging

- Use `tracing` with the `env-filter` feature on `tracing-subscriber`; the node reads the level
  from `RUST_LOG`, defaulting to `info`.
- `engine-core` does not log: its events are its output. The node logs around them.
- One log-visible correlation span per unit of work, carrying its id; long-running loops get none.
- A spawned future does not inherit the current span: spawn `fut.instrument(span)` or `fut.instrument(Span::current())`, or its lines lose the id. Never hold a `span.enter()` guard across an `.await`.
- Ids go in their own fields: `info!(account = %w.account, nonce = w.nonce, "…")`, never only inside the message string.
- Never log secrets, keys, or raw request bodies that may carry signatures.

## Dependencies

- Reuse before reinventing: don't reimplement well-known algorithms — use a permitted crate or search crates.io first.
- Prefer logic from the workspace's own crates and follow DRY: protocol types and maths live in `engine-core`, and the node reuses them.
- May add smaller utility crates freely; ask before adding a major crate, weighing maintenance and popularity.
- A new dependency of `engine-core` must be deterministic and must not pull in IO or a runtime.
- Permitted by default (non-exhaustive): `anyhow`, `axum`, `base64`, `bytes`, `chrono`, `chrono-tz`, `clap`, `futures`, `hex`, `http`, `itertools`, `k256`, `rand` (node only, careful outside tests), `regex`, `reqwest`, `serde` (and ecosystem), `sha2`, `sha3`, `smallvec`, `thiserror`, `tokio` (and ecosystem), `toml`, `tracing`, `tracing-subscriber` (with `env-filter`).

## Workspace

- Add dependencies to the workspace `Cargo.toml`, then reference them per package with `{ workspace = true }`.
- Protocol logic lives in library crates (`engine-core`); a binary crate wires IO, tasks, and
  configuration around them.

## File structure and layout

Each source file is laid out in this order:

1. Standard library imports
2. External crate imports
3. Local imports
4. Constants and statics
5. Type definitions
6. Trait definitions
7. Struct definitions
8. Trait method implementations
9. Struct method implementations
10. Functions
11. Unit tests

Import sections appear in this order, separated by a blank line. In `lib.rs` / `main.rs`, list module declarations first, then imports.

- Import only what you need, using absolute paths (`use crate::module::function`, not `use super::function`). Same for usage: `use tracing::info;` then `info!(...)`, unless one or two calls read better qualified.
- Test-only imports belong inside the `#[cfg(test)] mod tests { … }` block, not at module top level as `#[cfg(test)] use …;`. Inside test modules and test files, `use super::*` is fine.
- Organize source logically: a small module is a single file in `src`; a larger one gets its own subdirectory with a `mod.rs`.
- `impl Engine` may span several files (`engine.rs` for dispatch, time, funds, and admin; `execution.rs` for orders, pricing, and risk) when each holds one cohesive area.

## File line cap and module splits

Rust source files are capped at 500 lines. Split before a file crosses the cap rather than exempting it.

### How to split

Convert `foo.rs` to `foo/mod.rs` (a git rename) and extract cohesive units into
sibling submodules — never arbitrary halves. Established extractions:

- `tests.rs` — the inline `#[cfg(test)] mod tests { … }` body, always the first
  choice when tests dominate the count.
- `validation.rs` — `validate` functions of a config or spec type.
- `setup.rs` — construction and wiring pulled out of a process/loop module.

Moves are verbatim: no logic, doc-comment, or naming edits ride along — only
visibility and imports change. Keep each extracted file's std / external / local
import grouping.

### Visibility and `use` changes

- Extracted items get the narrowest visibility that compiles: `pub(super)` or `pub(crate)` for
  parent-only use; the submodule itself stays private (`mod setup;`) unless its API is
  deliberately public.
- A parent may glob-import or glob-re-export its own split submodules (`pub use pricing::*;`).
  Globs are avoided everywhere else; explicit names with absolute `crate::…` paths otherwise.
- After the move, prune imports the parent no longer uses (`clippy -D warnings` fails on
  leftovers).

### Tests layout

- Declare `#[cfg(test)] mod tests;` at the bottom of the parent, where the inline module sat.
- `tests.rs` starts with `use super::*;` plus what it needs; helpers go below the tests under a
  `// ── Helpers ──` divider.
- Protocol behaviour tests go through `Engine::apply` in `crates/core/tests/`, driven by
  sequenced commands as a client and the feeds would send them.

### Docs and verification

- Sync [agents/AGENTS.md](../../AGENTS.md) structure bullets and any crate `README.md` to the new
  paths; describe each file exactly once.
- Before pushing, verify the split was pure motion — diff the removed vs re-added lines;
  residuals must be only rewrapping, visibility, and imports.

## Crate README

When a crate gets a `README.md`, it has these sections, in order: H1 title (crate name), `## Overview`, `## Features` (bullets, no trailing period per item), `## Repository Structure`, `## Usage`, `## Configuration` (config keys and env vars; omit the section if none), `## Testing`, `## License` (single line: Apache-2.0). Deviate only when asked or clearly justified — ask first.

## Cargo profiles

- Release (workspace `Cargo.toml`): `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, and
  `overflow-checks = true`. Never turn overflow checks off: money arithmetic must halt on
  overflow, never wrap.
- Dev: the Cargo defaults; add `opt-level` overrides only for a measured need.

## Comments and rustdoc

- Each module opens with a `//!` comment that says what it owns and the protocol rule it
  implements, in a few sentences.
- Rustdoc every public item whose behaviour its name and signature do not already make clear;
  say what it guarantees, not how it works line by line.
- For a fallible function, name the error cases when the error type does not already enumerate
  them; a `Reject` variant documents its own case.

General comment terseness applies too — see [../../rules/code-comments.md](../../rules/code-comments.md).
