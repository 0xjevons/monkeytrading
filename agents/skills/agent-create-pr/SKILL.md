---
name: agent-create-pr
description: >
  Use when opening a pull request — "create a PR", "open a PR for this branch", "raise a PR for
  this work". Covers opening every PR as a draft, the preconditions for marking it ready
  (including a multi-agent agent-code-review at medium or higher), the issue blocker check, the
  `[scopes] Title` and the body in squashed-commit format ending with `Closes #12` or
  `Part of #12`, and watching the checks after it opens. Does not review, approve, or merge.
---

# agent-create-pr

Open one pull request for a feature or fix branch, in a shape a reviewer can act on: a title that
threads the branch slug and a body that says *why* before *what*. The diff already shows *what*
changed — the body exists to say *why*. Branches, commits, local checks, and pushes follow
[git-workflow](../../rules/git-workflow.md).

## Before opening any PR

- **Branch on a fresh base and pushed**, per git-workflow.
- **Committed** with approved, signature-free messages, per git-workflow.
- **No open blockers.** For each issue in the branch name, read what blocks it: the issues it is
  blocked by, and any `Blocked by #<n>` line in its body. A blocker that is not closed as completed
  is open. For any open blocker, stop and ask the user whether to open the PR anyway, naming each
  blocker and its state. Only the user's explicit yes for this PR proceeds; a standing instruction
  to work without approval does not cover it. If the blockers cannot be read, say so and ask the
  same question.

  ```bash
  gh api "repos/{owner}/{repo}/issues/$ISSUE/dependencies/blocked_by" --jq '.[] | "#\(.number) \(.state)"'
  gh issue view "$ISSUE" --json body --jq .body | rg -o 'Blocked by #[0-9]+'
  ```

## Before a PR is ready for review

A draft may skip these and say in its overview what is unfinished; a ready PR, or a draft being
marked ready, may not.

- **Work finished.** No "fix it in review" leftovers.
- **Feature complete.** A feature PR delivers its issue's whole acceptance list, its dev scenario
  is on the issue, and the scenario's scripted check passes
  ([agent-build-feature](../agent-build-feature/SKILL.md)).
- **Self-reviewed.** The change was built to
  [development-checklist](../../rules/development-checklist.md); walk every applicable section
  against the final diff once more and fix what it finds.
- **Multi-agent reviewed.** [agent-code-review](../agent-code-review/SKILL.md) ran in multi-agent
  mode at level `medium` or higher on the final diff, so Claude and Codex reviewers cross-validated
  each other. Fix every accepted Critical, High, and Medium finding and run it again until a run
  accepts none; Low and Nit findings may remain.
- **Local checks** for the affected crates passed, per git-workflow; the full CI runs
  on the PR.

## Title

`[<scopes>] Description` — the commit subject format from git-workflow.

- Match the branch slug — one kebab-case idea threads slug → title → commit.
- No PR number and no `[WIP]` (use Draft).

## Body

The PR squashes into one commit, so its title and body become that commit message. Shape: a short
overview, then `-` bullets, then the issue line — nothing else. No test plan, no review
checklist, no restatement of the diff.

- **Overview** — a few sentences on *why*: the cause and the constraint that shapes the design,
  whatever the diff cannot say for itself. Drop it when the bullets already carry the whole story.
- **Bullets** — logical changes, one line each, with the why where it is not obvious. Anything the
  diff cannot show gets its own bullet: a required env or secret change, an ordering constraint
  against another PR or a deploy, a manual step.
- **End with the issue** when the PR is tied to one, one line per issue in the branch name:
  `Closes #12` when the merge finishes the issue, `Part of #12` when it does not.
- **End state only.** Describe what the branch does, never how it got there: no review history,
  alternatives considered, "X instead of Y", "no longer", or notes addressed to reviewers. Context
  a reviewer needs goes in a PR comment. The text follows
  [writing-style](../../rules/writing-style.md).

## Opening it

- Use `gh`, authenticated as the user, and target `main`.
- Open every PR as a **Draft**, and mark it ready (`gh pr ready`) only once
  [Before a PR is ready for review](#before-a-pr-is-ready-for-review) holds. A reference PR that
  needs no review stays a draft and says so in its overview; the title stays clean.
- Watch the checks afterwards. A red PR is not ready yet: read the first real failure (the rest are
  usually fail-fast cancellations), fix, push, re-check, then ask for review.

## Merging

Branch rules on `main` require two checks: `ci-passed`, which CI sets, and `merge-approved`, the
owner's go-ahead. A ready PR is not an approved one: it merges only after the owner says,
explicitly and for that PR, that it may merge. On that instruction, the session the owner gave it
to posts `merge-approved` as a commit status on the commit that was the PR's head when the owner
approved, after checking it still is. A commit pushed after the approval is not covered, so the PR
waits for the owner again. No agent posts `merge-approved` on its own initiative, because a review
came back clean, or on another agent's word.

## Rules

- Operate under [`AGENTS.md`](../../AGENTS.md); load the rules its index selects for this action.
- A feature never spans several PRs or stacked PRs. One PR may carry several related features or
  issues that are reviewed and shipped together.
- Never open from an unpushed or stale branch.
- Never open a PR whose issue has an open blocker without the user's explicit yes.
- Issues are read-only here: never create, edit, or close an issue without the user's explicit
  approval.
- No AI signatures anywhere — title, body, or commits.
- Do not merge, approve, or dismiss reviews here. Post `merge-approved` only as
  [Merging](#merging) describes.
