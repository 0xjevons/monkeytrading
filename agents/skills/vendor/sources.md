# Vendored skill sources

Load when reviewing provenance, pulling, or refreshing `skills/vendor/`.

## How vendoring works (pull-on-demand)

Vendored packs are **not** committed. Each pack is pinned to an upstream commit in
[`vendor.manifest`](vendor.manifest) and materialized on demand by [`pull.sh`](pull.sh),
which downloads the commit-pinned tarball and extracts it to `skills/vendor/<pack>/`.
Those pack directories are gitignored.

- Bootstrap a checkout: `agents/skills/vendor/pull.sh` pulls every missing pack; packs already
  present are left as-is.
- Pull one pack: `agents/skills/vendor/pull.sh <pack>`.
- Refresh to a newer upstream commit: bump the hash in that pack's `vendor.manifest` URL, then
  `agents/skills/vendor/pull.sh --force <pack>`.

`vendor.manifest` is the single source of truth for the commit each pack is pinned to. Because
packs are fetched from source at use time rather than copied into this repo, the repo stays small
and there is no redistributed-license footprint to maintain here.

## What is vendored here

- `ethskills/` — Austin Griffith's Ethereum skill pack
- `evm-audit-skills/` — the EVM deep-audit checklist pack
- `pashov-skills/` — Pashov Solidity audits, pre-audit scans, and Fizz fuzz-suite workflows
- `rust-skills/` — Actionbook's Rust skill pack

Each pack lands at `skills/vendor/<pack>/` after `pull.sh`; route into it only through the wrapper
skills ([rust-implementation](../rust-implementation/SKILL.md),
[smart-contract-solidity-implementation](../smart-contract-solidity-implementation/SKILL.md)),
never by loading the whole tree. Once a wrapper selects a vendored pack or routed vendored file,
that load is mandatory. If the pack or file is missing, run `agents/skills/vendor/pull.sh <pack>`,
retry the exact read, and stop with the missing path if it still fails. Do not substitute memory,
adjacent skills, or wrapper-only guidance.

## When to use the Pashov vendored pack

Use the Solidity skill's [Pashov routing and execution policy](../smart-contract-solidity-implementation/references/pashov.md)
only for an explicit request naming the workflow. It maps all five pinned upstream entrypoints
and requires a warning about agent usage/cost and approval of a concrete run plan; generic
reviews and fuzzing tasks do not activate these workflows.

The full upstream tree is retained so relative references, scripts, agent prompts, templates,
and the nested Fizz skills resolve. Load only the selected workflow. Upstream tool/model names
must be adapted to available capabilities; repository execution permissions still apply.

## Upstream sources

The live pinned commit for each pack is in [`vendor.manifest`](vendor.manifest). The repo,
branch, license, and scope below are the provenance.

### EthSkills

- Repository: `https://github.com/austintgriffith/ethskills`
- Default branch used: `master`
- Site endpoints imported into the local snapshot:
  - `https://ethskills.com/SKILL.md`
  - `https://ethskills.com/ship/SKILL.md`
  - `https://ethskills.com/why/SKILL.md`
  - `https://ethskills.com/protocol/SKILL.md`
  - `https://ethskills.com/gas/SKILL.md`
  - `https://ethskills.com/wallets/SKILL.md`
  - `https://ethskills.com/l2s/SKILL.md`
  - `https://ethskills.com/standards/SKILL.md`
  - `https://ethskills.com/tools/SKILL.md`
  - `https://ethskills.com/building-blocks/SKILL.md`
  - `https://ethskills.com/orchestration/SKILL.md`
  - `https://ethskills.com/addresses/SKILL.md`
  - `https://ethskills.com/concepts/SKILL.md`
  - `https://ethskills.com/security/SKILL.md`
  - `https://ethskills.com/noir/SKILL.md`
  - `https://ethskills.com/testing/SKILL.md`
  - `https://ethskills.com/indexing/SKILL.md`
  - `https://ethskills.com/frontend-ux/SKILL.md`
  - `https://ethskills.com/frontend-playbook/SKILL.md`
  - `https://ethskills.com/qa/SKILL.md`
  - `https://ethskills.com/audit/SKILL.md`
  - `https://ethskills.com/feedback/SKILL.md`

### Pashov Skills

- Repository: `https://github.com/pashov/skills`
- Default branch used: `main`
- License: MIT (`LICENSE` preserved)
- Local snapshot scope: whole repository, including `solidity-auditor/`, `x-ray/`, `fizz/`,
  and the nested `fizz/skills/fizz-sync/` and `fizz/skills/fizz-convert/` skills.
- Refresh: update the manifest pin, run `agents/skills/vendor/pull.sh --force pashov-skills`,
  and check the five routing targets and their relative resources against the new tree.

### EVM Audit Skills

- Repository: `https://github.com/austintgriffith/evm-audit-skills`
- Default branch used: `main`
- Master index imported:
  - `https://raw.githubusercontent.com/austintgriffith/evm-audit-skills/main/evm-audit-master/SKILL.md`
- Specialist checklist source pattern:
  - `https://raw.githubusercontent.com/austintgriffith/evm-audit-skills/main/<skill-name>/references/checklist.md`

### Rust Skills

- Repository: `https://github.com/actionbook/rust-skills`
- Default branch used: `main`
- Local snapshot scope: whole repository; the wrapper routes into `skills/` starting at
  `skills/rust-router/SKILL.md`.

## Refresh procedure

1. Find the new upstream commit (latest default branch):
   `git ls-remote https://github.com/<org>/<repo> HEAD`.
2. Update that pack's tarball URL in [`vendor.manifest`](vendor.manifest) to the new commit hash.
3. Re-pull: `agents/skills/vendor/pull.sh --force <pack>`.
4. If the upstream layout moved, update the pack's `subtree` column in the manifest and any
   wrapper routing paths that name vendored files.
5. Validate the wrapper routing targets resolve against the freshly pulled tree (`SKILL.md`,
   routing tables, checklists, module / reference paths).
6. If workflow guidance changed, update the affected wrapper: `../rust-implementation/SKILL.md`,
   or `../smart-contract-solidity-implementation/SKILL.md` and its
   `references/ethskills-routing.md` / `references/deep-audit.md` / `references/pashov.md`.
