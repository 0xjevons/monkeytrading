---
name: smart-contract-solidity-implementation
description: Use when designing, implementing, optimizing, testing, reviewing, or auditing EVM contracts. Covers architecture, Solidity, standard selection, gas, Foundry tests, pre-deploy review, and deep audits for tokens, vaults, staking, lending, governance, oracles, NFTs, bridges, and upgradeable systems.
---

# Smart Contract Implementation

Load on request for Solidity design, implementation, gas, tests, or security review.

## Start here

1. Use `../../AGENTS.md` as the index — load only triggered rule files.
2. State assumptions before editing: asset types, decimals, standards, upgradeability, target chain, admin model, oracle model, external deps.
3. Keep MVP small. Default 1–2 contracts. Push back on factories, cross-chain scope, or onchain storage that doesn't protect value or commitments.
4. For every state transition: who calls it? why pay gas? what breaks if nobody calls it?
5. If `../vendor/ethskills/` is absent, run `../vendor/pull.sh ethskills` before loading vendored
   Ethskills pages.
6. For deep audits, if `../vendor/evm-audit-skills/` is absent, run
   `../vendor/pull.sh evm-audit-skills` before loading vendored audit checklists.
7. If a routed vendored Solidity/audit file is missing, run the matching pull, retry the exact
   read, and stop with the missing path if it still fails; do not continue with only this wrapper's
   guidance.
8. Read [references/ethskills-routing.md](references/ethskills-routing.md); load only the vendored upstream pages relevant to the task.
9. Read [references/sources.md](references/sources.md) before resyncing. For the vendored packs themselves, use [../vendor/sources.md](../vendor/sources.md).

## This repo's contracts

The chain holds custody only; execution happens in the enclave. A contract here exists to hold
USDC, credit deposits the engine can see, and pay out withdrawals the engine approved.

- **Trusted signer, stated.** The vault honours withdrawals signed by the attested enclave key and
  nothing else. That is the one accepted operator dependency (see Design below). Every design names
  how the signer is set and rotated, who may change it, and what users can do if the enclave stops
  signing — an escape hatch, such as a delayed withdrawal against the last posted state root, is a
  core design question, not an extra.
- **Signatures match the engine.** Withdrawal and checkpoint digests must match what
  `crates/node/src/tee.rs` signs, byte for byte: domain tag, chain id, vault address, account,
  amount, and nonce. A change on either side changes both, in one PR, with a test that checks one
  against the other.
- **Amounts.** USDC has 6 decimals and the engine's `Fixed` has 9. Conversion happens at the
  custody boundary, is checked, and never rounds in the user's favour on a payout.
- **Deposits are events.** Deposit events carry what the custody adapter needs to credit the right
  account exactly once: account, amount, and a unique reference (transaction hash and log index).
  A deposit sequenced twice under the same reference is a double credit, so crediting must be
  idempotent on that reference.

## Optional Pashov workflows

For explicit requests naming Pashov Solidity Auditor, Pashov X-Ray, Fizz, Fizz Sync, or Fizz
Convert, read [Pashov routing and execution policy](references/pashov.md). These workflows
can use many agents and substantial compute; warn and obtain approval of a concrete run plan
before execution. Generic implementation, testing, or audit requests keep the workflow below.
Do not combine Pashov and deep-audit passes unless both were requested and approved.

## Workflow

### 1. Design

- Onchain logic = ownership, transfers, accounting, commitments, censorship-resistant rules. Nothing else.
- Prefer audited primitives (OpenZeppelin) — add only the custom state / math the product actually needs.
- Define before coding: contract list, privileged roles, value flows, external calls, events, upgrade plan, 3–5 core invariants.
- Reject designs needing timers / cron / trusted operator unless the dependency is explicit and accepted.

### 2. Implement

- Explicit units for decimals, basis points, fixed-point scales, time.
- Custom errors, `SafeERC20`, checks-effects-interactions, explicit access control.
- Emit events for every state change offchain must observe.
- Upgradeable → OZ upgradeable, `initializer`, append-only storage, multisig / timelock authority.
- Integration addresses configurable per deployment; never invent — verify real addresses first.

### 3. Optimize

- Read [references/gas-optimization.md](references/gas-optimization.md) before low-level work.
- Measure first: `forge test --gas-report`, `forge snapshot`.
- Architecture + storage writes before opcode tweaks.
- Re-run full test suite after every material optimization.

### 4. Test

- Read the `ethskills` testing guide via routing.
- Unit tests for custom logic + access control.
- Fuzz all math + user inputs.
- Fork-test every external integration.
- Invariant tests for stateful protocols (vaults, AMMs, lending, rewards).
- Don't test getters or OZ internals.

### 5. Security review

- Pre-deploy review even on small contracts.
- Focus: decimals, precision loss, reentrancy, oracle safety, approvals, fee-on-transfer / rebasing quirks, signature replay, upgrade safety, DoS / gas griefing.
- Short threat model: assets at risk, privileged actors, trust assumptions, kill switches, known non-goals.

### 6. Deep audit

- Read [references/deep-audit.md](references/deep-audit.md) for audits, third-party review, or final sign-off.
- Separate implementation from audit — fresh reviewer / fresh context preferred.
- Always include general + precision-math passes; add domain-specific passes per contract type.
- Findings first, ordered by severity, with location, exploit / failure mode, concrete remediation.

## Deliverables

- **Design**: architecture note, state-transition table, invariants, open questions.
- **Implementation**: code + tests + gas measurements + security notes on tradeoffs.
- **Audit**: findings first, then assumptions, then residual risk / missing coverage.

## Stop conditions

- Missing decimals / oracle / upgrade model / privileged roles.
- Unbounded loops over user-controlled data without a hard cap or pull-based alternative.
- Spot-price oracle for value-critical logic.
- Time-based behavior with no caller incentive.
- Gas "optimizations" reducing readability / safety without measured gain.
