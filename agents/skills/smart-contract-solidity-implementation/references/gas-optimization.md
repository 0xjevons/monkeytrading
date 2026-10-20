# Gas optimization

Load on request when reducing deployment cost or runtime gas after the design is correct and tests already
cover the behavior.

## Measurement First

- Capture a baseline before editing: `forge test --gas-report` and `forge snapshot`.
- Optimize hot paths, user-facing flows, and loops first. Ignore cold admin paths unless they are
  provably a problem.
- Re-measure after each meaningful change. Keep every optimization tied to an actual delta.

## Highest-Value Wins

- Remove storage writes before chasing opcode-level savings.
- Prefer fewer contracts and fewer external calls over clever local tricks.
- Use `constant` and `immutable` for fixed values.
- Prefer `calldata` over `memory` for external read-only arguments.
- Cache repeated storage reads in local variables.
- Use custom errors instead of long revert strings.
- Pack storage only when fields share a slot cleanly and are usually read or written together.
- Use events instead of storage for history that only offchain consumers need.
- Replace push loops with pull claims, pagination, or bounded batches.

## Safe Micro-Optimizations

- Short-circuit invalid input before expensive work.
- Keep structs tight and avoid duplicated derived state.
- Prefer `++i` style loops and unchecked increments only when bounds are proven and tested.
- Use `unchecked` arithmetic only around proven-safe operations and only with direct tests around
  the bounds.
- Consider assembly only after measurement, with comments, targeted scope, and equivalent tests for
  the optimized path.

## Red Flags

- Hand-rolled bit tricks that save little and hide intent.
- Replacing audited libraries only to shave a small amount of gas.
- Adding upgradeability or clone factories for a single deployment.
- Gas changes that weaken validation, logging, or security assumptions.

If the cheapest code is less safe, ship the safer code and document the gas tradeoff.
