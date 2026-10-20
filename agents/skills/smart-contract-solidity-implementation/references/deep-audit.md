# Deep audit

Load on request when doing a final security review, auditing third-party contracts, or running a separate
internal sign-off after implementation.

## Source of Truth

Load these before starting:

1. `../../vendor/ethskills/audit/SKILL.md`
2. `../../vendor/evm-audit-skills/evm-audit-master/SKILL.md`

The vendored `ethskills` audit page is the entry point. The vendored `evm-audit-master` file is the
routing table and methodology for the specialist checklists. Do not load the full audit checklist
tree; select only the always-on and domain-relevant checklist files below.

## Always-On Audit Passes

- `evm-audit-general`
- `evm-audit-precision-math`

Checklist paths:

- `../../vendor/evm-audit-skills/evm-audit-general/references/checklist.md`
- `../../vendor/evm-audit-skills/evm-audit-precision-math/references/checklist.md`

## Add Domain Passes When Relevant

- ERC-20 interactions, collateral, approvals, fee-on-transfer or rebasing tokens:
  `../../vendor/evm-audit-skills/evm-audit-erc20/references/checklist.md`
- AMMs, routers, pools, hooks, swaps, CLMs:
  `../../vendor/evm-audit-skills/evm-audit-defi-amm/references/checklist.md`,
  `../../vendor/evm-audit-skills/evm-audit-flashloans/references/checklist.md`
- Lending, borrowing, CDPs, liquidations:
  `../../vendor/evm-audit-skills/evm-audit-defi-lending/references/checklist.md`,
  `../../vendor/evm-audit-skills/evm-audit-oracles/references/checklist.md`,
  `../../vendor/evm-audit-skills/evm-audit-flashloans/references/checklist.md`
- Vaults and share-price logic: `../../vendor/evm-audit-skills/evm-audit-erc4626/references/checklist.md`
- Staking or restaking: `../../vendor/evm-audit-skills/evm-audit-defi-staking/references/checklist.md`
- Upgradeable contracts or proxies: `../../vendor/evm-audit-skills/evm-audit-proxies/references/checklist.md`
- Signatures, permits, meta-transactions: `../../vendor/evm-audit-skills/evm-audit-signatures/references/checklist.md`
- Governance, timelocks, multisig controls:
  `../../vendor/evm-audit-skills/evm-audit-governance/references/checklist.md`,
  `../../vendor/evm-audit-skills/evm-audit-access-control/references/checklist.md`
- NFTs or token-bound asset flows: `../../vendor/evm-audit-skills/evm-audit-erc721/references/checklist.md`
- Price oracles, TWAPs, external data feeds, verifiable randomness: `../../vendor/evm-audit-skills/evm-audit-oracles/references/checklist.md`
- Assembly, Yul, CREATE2, low-level calls: `../../vendor/evm-audit-skills/evm-audit-assembly/references/checklist.md`
- L2 or non-mainnet deployment: `../../vendor/evm-audit-skills/evm-audit-chain-specific/references/checklist.md`
- Unbounded loops, batch operations, return-data griefing: `../../vendor/evm-audit-skills/evm-audit-dos/references/checklist.md`

## Audit Workflow

1. Recon the codebase: inheritance, proxy boundaries, storage layout, privileged roles, external
   calls, token flows, and deployment chain.
2. Select only the relevant specialist checklists.
3. Run each checklist as an independent review pass. If the runtime supports delegation, parallelize
   them. If not, run them sequentially but keep findings separated by checklist.
4. Deduplicate overlapping findings and look for combined attack paths across categories.
5. Publish a final report with findings ordered by severity.

## Finding Format

Every finding should include:

- Severity
- Category
- Location
- Description
- Proof or failure mode
- Recommendation

Severity rules:

- Critical: direct third-party fund loss with no meaningful precondition
- High: fund loss or permanent DoS under realistic but narrower conditions
- Medium: incorrect accounting, trust-model break, owner-only loss, or material degradation
- Low: latent bug, bad practice, or confusing behavior without direct fund risk
- Info: useful note with no concrete security impact

Deep audit is not a substitute for the implementation-stage security review. Use both.
