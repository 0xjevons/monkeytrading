# Pashov workflows

Read only for an explicit request naming one of these workflows. This reference extends
[the Solidity skill](../SKILL.md); local instructions take precedence over upstream instructions.

## Upstream routes

After preparing and approving the run plan below, use the selected route. Read only the
instructions needed to prepare that plan before approval.

| Workflow | Purpose | Pinned upstream entrypoint |
| --- | --- | --- |
| Pashov Solidity Auditor | Security audit of contracts and deployment scripts | [solidity-auditor](../../vendor/pashov-skills/solidity-auditor/SKILL.md) |
| Pashov X-Ray | Pre-audit report, threat model, and invariants | [x-ray](../../vendor/pashov-skills/x-ray/SKILL.md) |
| Pashov Fizz | Stateful fuzz-suite generation | [fizz](../../vendor/pashov-skills/fizz/SKILL.md) |
| Pashov Fizz Sync | Reconcile existing harness drift | [fizz-sync](../../vendor/pashov-skills/fizz/skills/fizz-sync/SKILL.md) |
| Pashov Fizz Convert | Implement existing Fizz properties as assertions | [fizz-convert](../../vendor/pashov-skills/fizz/skills/fizz-convert/SKILL.md) |

From the repo root, run `agents/skills/vendor/pull.sh pashov-skills` if the pack is
missing. If a routed file is missing, run the pull, retry that exact read, and stop if it
still fails. Resolve relative resources and `SKILL_PATH` / `SKILL_DIR` from the selected
vendored skill directory. Use the requested contract package as `PROJECT_ROOT` and preserve
any explicit file scope. Load only resources needed by the selected workflow.

For the auditor, set `{resolved_path}` to the vendored `solidity-auditor/references/`
directory explicitly; it is gitignored and may be skipped by default glob discovery.

## Activation and approval

For repository path-based loading, the explicit-request prose below is the invocation gate;
skill metadata does not universally disable invocation across consumers.

- Activate only for an explicit request naming Pashov Solidity Auditor, Pashov X-Ray,
  Fizz, Fizz Sync, Fizz Convert, or the upstream skill name `solidity-auditor`, `x-ray`, `fizz`,
  `fizz-sync`, or `fizz-convert`. Do not activate from generic security review, audit,
  pre-audit, fuzzing, or implementation requests.
- Installation, maintenance, and reading these instructions do not authorize execution.
  Never run a Pashov workflow as a smoke test of its installation.
- Before execution, read only the instructions and project configuration needed to prepare
  a concrete plan. Do not start scanning, upstream scripts, subagents, builds, or campaigns.
- Present this warning: **Pashov workflows can use many agents and substantial tokens,
  time, and compute. The pinned Solidity Auditor specifies 12 agents per pass; additional
  passes multiply that work. Fizz also delegates multiple phases.** State the actual planned
  count for the selected workflow; do not imply every workflow uses 12 agents.
- Show the skill, package/files, pass count, agent roles/count (or a bounded maximum),
  providers/models, concurrency, output paths, mutations, commands, and campaign time limits.
  Separate harness generation, builds/tests, and fuzz campaigns. Include local servers or
  browser viewers if an upstream helper starts them. Do not invent a dollar estimate.
- Ask for explicit approval of that plan and wait. Name and link this
  reference and quote its approval requirement so the reason for the question is clear.
  Merely naming the skill, `--automatic`, `--loop`, or asking for a review is not approval.
- Approval applies only to that plan and run, including its listed phases and agents.
  Do not re-ask within an approved plan. New runs, increased scope/counts, extra passes,
  model/provider substitutions, or additional commands/mutations require renewed approval.
  Declining or not answering means do not execute; do not silently substitute another audit.

## Routing and execution

- Ordinary Solidity work stays with `smart-contract-solidity-implementation`; ordinary
  branch/PR review stays with `agent-code-review` and its current-agent default.
  Do not automatically chain Pashov and existing deep-audit passes or one Pashov skill
  into another. Each additional workflow must be named in the approved plan.
- Map upstream Read/Glob/Grep/Bash to available file/search/shell tools, TodoWrite to
  available progress tracking (or a concise status list), and AskUserQuestion to the
  available user-input mechanism. Missing UI tools do not waive approval.
- Map Agent to native delegation when available. For independent CLI agents, load
  `../../ask-agent/SKILL.md` and use its dispatch and session-recording protocol.
  Resolve the actual available models before approval; do not silently map Sonnet/Opus
  names to a different provider or model. If a capability is missing, report the blocker.
- Batch independent roles within the smaller of the approved concurrency and runtime
  capacity. Preserve upstream phase dependencies. Do not add providers, review-level
  pairs, retries, or loop passes beyond the approved agent budget.
- Give every delegate the approved scope, permitted reads/writes/commands, output path,
  and remaining budget. Delegates cannot recursively expand the agent roster.
- Upstream automatic mode operates only within the approved plan. Stop at unapproved
  build/test/campaign/setup steps; do not install tools or change configuration implicitly.
  Do not execute deployment scripts while auditing them.

## X-Ray acquisition override

This replaces upstream Fizz's entire X-Ray acquisition protocol, including global installation
and automatic Protocol Analyzer fallback. Reuse `<PROJECT_ROOT>/x-ray/x-ray.md` when present.
Otherwise, include the pinned [Pashov X-Ray workflow](../../vendor/pashov-skills/x-ray/SKILL.md), its agents,
commands, and output path in the approved run plan before executing it. Stop if the required
report cannot be obtained within that approval; do not install a global skill or substitute
the Protocol Analyzer fallback.

## Echidna configuration override

Preserve the package's existing `echidna.yaml`, including during scaffold generation and
library-linking repairs. As an approved preparation step, create
`<PROJECT_ROOT>/<META_DIR>/echidna.yaml` from the pinned Fizz `templates/echidna.yaml`.
If the Fizz config already exists, review and reuse it instead of overwriting it with the
template. Otherwise, use an absolute `PROJECT_ROOT`, the vendored Fizz directory as
`SKILL_PATH`, and the selected package-relative `META_DIR` (default `fizz_data`):

```sh
mkdir -p "$PROJECT_ROOT/$META_DIR"
cp "$SKILL_PATH/templates/echidna.yaml" "$PROJECT_ROOT/$META_DIR/echidna.yaml"
```

Retain `testMode: "assertion"` and `prefix: "property_"`; set `corpusDir` to the absolute
`<PROJECT_ROOT>/<META_DIR>/corpus_echidna` path to isolate Fizz from existing corpora. Apply
approved package-specific adjustments, including library linking, only to this Fizz config.

Always pass this config's absolute path through `--config`, including in generated reports
and reproduction commands. These commands replace upstream's default-config examples and
require campaign approval (use the approved timeout):

```sh
node "$SKILL_PATH/scripts/run_echidna.js" "$PROJECT_ROOT" --meta-dir "$META_DIR" --config "$PROJECT_ROOT/$META_DIR/echidna.yaml" --timeout "$TIMEOUT_SECONDS"
cd "$PROJECT_ROOT"
echidna . --contract FuzzTester --config "$PROJECT_ROOT/$META_DIR/echidna.yaml"
```

## Artifacts and updates

- Store orchestration prompts, agent outputs, session IDs, and approval/scope records under
  `agents/.processes/pashov/<timestamp>-<workflow>/` relative to the repo root.
- Keep upstream `.audit-*` scratch directories and `.solidity-auditor/` run/memory files
  package-local and gitignored. Preserve the upstream paths used by its report assembler.
- Keep Fizz harness source, `PROPERTIES.md`, configuration, and metadata needed by sync/convert
  reviewable in the selected package. Ignore only runtime corpora/logs and Slither cache; do not blanket-ignore
  `test/fizz/` or `fizz_data/`. Review generated changes before proposing a commit.
- Keep X-Ray reports package-local by default. Include their paths in the plan and distinguish
  intentional report deliverables from temporary execution artifacts.
- Use the pinned vendored instructions. Do not auto-update from upstream version notices;
  updates go through the manifest and a review of routing, permissions, and resource paths.

For provenance or updates, see [vendored sources](../../vendor/sources.md). Refresh through
`vendor.manifest` and `pull.sh --force pashov-skills`; keep the upstream pack unmodified.
