---
name: agent-build-feature
description: >
  Use when implementing a feature — "build #12", "implement this feature", "start the next
  feature". Confirms the feature is ready to build, builds all of it on one branch for one PR
  (parallel subagents allowed), self-reviews it with agent-code-review, opens the PR through
  agent-create-pr, and runs its dev scenario after merge. Deploys and issue writes need the
  user's approval.
---

# agent-build-feature

Build one ready feature end to end on one branch, so the team reviews it once, tests it in dev, and
promotes it to prod. Features are planned with [agent-plan-feature](../agent-plan-feature/SKILL.md);
a bug fix or a refactor that is not a feature goes straight through
[git-workflow](../../rules/git-workflow.md) and [agent-create-pr](../agent-create-pr/SKILL.md).

## Before the first edit

- **Ready.** Read the feature issue (`gh issue view <n> --comments`). A planned feature carries the `ready-to-build`
  label, every blocker is done, and its acceptance criteria cite their sources. If a decision is
  open, stop and name it. Build nothing of a feature that is not ready, not even the parts that look
  settled.
- **An issue the plan did not write.** Check the same things: every decision its design needs is
  made, its criteria have sources, and it has a dev scenario. Name each gap and ask the user to fill
  it or confirm the issue is ready; never decide that alone.
- **Whole scope.** List what the feature needs to run in dev: protocol rules, log and state-root
  compatibility, adapters, API, config and secrets, contracts, monitoring. A backend feature is
  complete when its dev scenario runs through its API, an operator command, or a scripted check;
  app work is its own feature. Anything the issue does not cover is a question for the user now, not a later PR.
- **Size.** Estimate the PR from that scope, counting production code and tests as
  [agent-plan-feature](../agent-plan-feature/SKILL.md#what-a-feature-is) does, and again when
  planning the build (the interfaces the subagents build against) shows more than the issue
  suggested. When the issue records why a large feature has no seam and the estimate has not grown
  past its size, build it. Otherwise, past about 5k lines, look for a seam as agent-plan-feature
  describes and propose any split to the user before the first edit; past about 10k, build one PR
  only when they agree there is no seam. An agreed split changes the plan: re-plan through
  agent-plan-feature, then build the first resulting feature once its issue is ready.
- **Branch.** One pushed branch for the feature, named for its issue per git-workflow.
- Load [think-before-coding](../../rules/think-before-coding.md),
  [development-checklist](../../rules/development-checklist.md), and the `*-implementation` skill
  for every area the feature touches.

## Building

- Build the narrowest end-to-end path first, then widen it, so the branch runs at every step.
- Parallel work is fine: split by area across subagents, each in its own worktree on a local
  branch, and merge each into the feature branch. Never push a branch or open a PR per part, and
  never stack PRs for one feature.
- Map every acceptance criterion to code and a test. Test where the behaviour lives: unit tests for
  rules (`engine-core` behaviour goes through `Engine::apply`), integration tests at the chain,
  feed, and enclave boundaries, and a scripted check that runs the feature's dev scenario.

## Before review

- Run the local checks per git-workflow.
- Self-review with [agent-code-review](../agent-code-review/SKILL.md), multi-agent at `medium` for a
  large feature, and fix what it finds before asking for human review.
- Show the user the dev scenario — steps, expected result, and the config it needs — and on their
  yes write it into the issue's dev scenario section, replacing the planned text.
- The scenario's scripted check passes in CI.
- Put what promotion needs in the PR body: config, secrets, genesis or parameter changes, contract
  deployments, deploy ordering, rollback.

## Merge and dev run

- Open the PR with agent-create-pr, ending `Part of #<feature>`: the feature closes after its dev
  run, not at merge.
- Answer review with [agent-address-pr-comments](../agent-address-pr-comments/SKILL.md), aiming for
  one round.
- After merge, dev runs the `main` build: a local node built from `main`, or a hosted dev
  environment once one exists, deployed only on the user's explicit approval
  ([no-deploy](../../rules/no-deploy.md)). Never run the dev scenario on a branch build.
- Run the dev scenario in dev, show the user the result, and on their yes record it on the issue
  and close it.
- Promotion to prod needs its own approval.

## Rules

- One feature never spans several pushed branches or PRs.
- Never build part of a feature that is not ready.
- Never write to a GitHub issue without the user's yes to the exact text.
- Never deploy or change an environment without explicit per-action approval.
