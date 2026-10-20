# EthSkills routing

Load on request when a smart-contract task needs the vendored Ethereum reference set under
`skills/vendor/ethskills`.

Load only the local files relevant to the task:

| Need | Fetch |
| --- | --- |
| Architecture, contract count, what belongs onchain | `../../vendor/ethskills/ship/SKILL.md` |
| Incentives, state-machine design, "nothing is automatic" | `../../vendor/ethskills/concepts/SKILL.md` |
| Standard selection and live protocol standards | `../../vendor/ethskills/standards/SKILL.md` |
| Tooling, Foundry stack, explorers, agent-oriented Ethereum tools | `../../vendor/ethskills/tools/SKILL.md` |
| Current gas costs and mainnet vs L2 tradeoffs | `../../vendor/ethskills/gas/SKILL.md` |
| Defensive Solidity patterns and pre-deploy checklist | `../../vendor/ethskills/security/SKILL.md` |
| Foundry unit, fuzz, fork, and invariant testing | `../../vendor/ethskills/testing/SKILL.md` |
| Audit routing and deep-audit entry point | `../../vendor/ethskills/audit/SKILL.md` |

Common implementation route:

Start with `../../vendor/ethskills/ship/SKILL.md`, then add only the files triggered by the actual
contract shape:

- State-machine or caller-incentive questions: `../../vendor/ethskills/concepts/SKILL.md`
- Standards selection: `../../vendor/ethskills/standards/SKILL.md`
- Defensive implementation or pre-deploy review: `../../vendor/ethskills/security/SKILL.md`
- Foundry unit/fuzz/fork/invariant tests: `../../vendor/ethskills/testing/SKILL.md`
- Gas measurement or chain-cost tradeoffs: `../../vendor/ethskills/gas/SKILL.md`

Add these when needed:

- External protocol integration: `../../vendor/ethskills/building-blocks/SKILL.md`
- Verified protocol addresses: `../../vendor/ethskills/addresses/SKILL.md`
- Chain choice or L2 deployment: `../../vendor/ethskills/l2s/SKILL.md`
- Wallet, multisig, or production ownership setup: `../../vendor/ethskills/wallets/SKILL.md`
- Final security review: `../../vendor/ethskills/audit/SKILL.md`

Treat the vendored files as a local snapshot. If they need refreshing, use
[sources.md](sources.md); do not switch the workflow back to live runtime fetches, and do not load
the full vendored `ethskills` tree.
