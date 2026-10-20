---
name: agent-review-pr
description: >
  Never submits a review: stages a PENDING review only after the user's yes. Use when asked to
  review or re-review a GitHub PR — "review PR 42", "agent-review-pr", "review-pr", "PR draft
  review", "draft review", "draft PR comments", "review the PR but don't submit", "re-check my
  review", "follow up on my review", "agent-review-pr multi-agent", or "agent-review-pr high".
  Every run reviews the current head with `agent-code-review` (current-agent by default,
  multi-agent on request), rechecks every Low-confidence finding until it is confirmed or dropped,
  skips points the PR's discussion already makes, drafts the rest (Severity, Complexity,
  Confidence, Issue, Suggested fix), and on a re-review also rechecks the user's earlier threads;
  it shows one report with a verdict and asks "Push the draft comments?", and only a yes posts.
  For a local report with no GitHub writes, use `agent-code-review`.
---

# agent-review-pr

Review a PR without submitting a GitHub review. Every run — first review or re-review — drafts
comments locally, prints the [report](templates/report.md) **to the user only** — why the PR
exists, what it changes, its fit to the issue, risk, tests, verdict, every drafted and dropped
comment, the review body's overview and verdict, and what was not verified — and ends
with **"Push the draft comments?"**. Only a yes
posts, and the answer carries the draft review's link. A re-review is the same run plus one step:
it rechecks the comments you already left and drafts a resolve, reply, or reopen for each, or
leaves it to the user's decision.

## Entry and routing

- Use an explicit PR (`#42`, `PR 42`, or URL), or the open PR for the current branch. Stop if
  branch mode finds none; do not create a PR.
- Use `gh`, resolve `gh api user --jq .login`, then load
  [no-guessing-current-state](../../rules/no-guessing-current-state.md) before every live-state
  decision.
- A pending review owned by that identity means stop: report its link and ask the user to submit
  or delete it in GitHub. This skill does neither.
- Otherwise follow [references/review.md](references/review.md). It is a re-review when that
  identity already has a submitted review or owns a review-thread root comment on the PR.

## Review modes

The mode only selects how `agent-code-review` runs; filtering and posting stay the same.

- **Current-agent (default):** `agent-code-review` current-agent mode.
- **Multi-agent:** "agent-review-pr multi-agent", "multi-agent PR review", or any review level
  (`low`, `medium`, `high`, or several) runs `agent-code-review` multi-agent mode with exactly the
  requested levels. Without a level, that mode's default applies (`medium`). Reject an invalid
  level rather than changing review depth.

## Findings

[agent-code-review](../agent-code-review/SKILL.md), walking
[development-checklist](../../rules/development-checklist.md), is the only source of findings.
A kept finding that becomes a new inline comment posts five lines exactly as written in
`code-review-final.md`: `Severity`, `Complexity`, `Confidence`, `Issue`, and `Suggested fix`,
through [templates/inline-comment.md](templates/inline-comment.md). The suggested fix is one option
for the author to weigh, not an instruction; `agent-address-pr-comments` treats it that way. The
title, scenario, reporters, validators, and file stay in the local report. A finding that becomes
a reply in an existing thread follows
[step 5 of the review](references/review.md#5-match-findings-to-the-discussion) instead. It never
rewords, merges, or re-grades the severity of a finding.

Keep a finding only when all hold:

- **It stays inside the PR's scope**: the PR introduces, changes, or worsens the problem. Drop a
  finding that asks for work the PR does not otherwise need — a pre-existing issue in untouched
  code, a refactor, speculative hardening, a nice-to-have — unless it names a concrete way this
  PR's change breaks or depends on it.
- **Its confidence is High or Medium.** Recheck every `Low` finding in detail against the code: one
  the code confirms gets its confidence raised to what the evidence supports; one it disproves, or
  cannot settle, is dropped. No `Low` finding is posted.
- **The PR's discussion does not already make the point.** A finding someone already raised is
  dropped when it adds nothing, and becomes a reply in that thread when it adds a scenario,
  evidence, or a fix direction.

Severity does not filter.

## Push gate

Nothing reaches GitHub until the user says yes:

1. Build and validate `review.json`, then print [templates/report.md](templates/report.md) filled
   in; its review body, draft comments, and thread actions are the push preview.
2. The report ends by asking exactly: **Push the draft comments?** Wait for the answer.
3. **Yes** → re-check live state, stage the new comments and every reply in one PENDING review,
   apply the resolves and reopens, and give the completion line. **No** → post nothing, and give
   the completion line.

With no new comments and no thread action there is nothing to push: skip the question and post
nothing. A request that already says to push the draft comments counts as the yes; the preview is
still shown.

## Non-negotiable rules

- Never submit: never use `gh pr review` or an `event` field. Post only after the push gate's yes,
  and never turn the review into `APPROVE`, `REQUEST_CHANGES`, or `COMMENT` on GitHub.
- A new comment posts only the five lines of a kept finding, unchanged, built with the template and
  `jq --rawfile` and validated first; never hand-write the JSON or post a dropped finding.
- Resolve or reopen only threads the active identity started. Reply to any thread only when the
  reply adds information it lacks; never repeat a point already made.
- Replies follow [writing-style](../../rules/writing-style.md). No AI signature in any comment,
  reply, or review body.
- Never print or persist a secret value.
- Review in a clean worktree at the recorded head. Never edit, stage, or commit there; remove only
  a worktree this run created.
- Every point that can anchor to a diff line goes inline, nits and short notes included: an inline
  thread can be replied to and resolved on its own, a paragraph in the review body cannot. The
  body, built from [templates/review-body.md](templates/review-body.md), holds an `Overview` of the
  review — what it found, not what the PR does — in one or two sentences that repeat no inline
  comment, then only what cannot anchor — a line outside the diff, the PR description, cross-PR
  context — and ends with the bare `Verdict`. No praise.
- This skill never fixes code, commits, pushes, merges, or deploys, and never changes an issue.
- A pushed run with new comments or replies but no draft-review link in its completion line is
  incomplete.

## Completion line

The report already carries everything else; after the push gate's answer, print:

- Yes → `**Draft review:** <link>` — `html_url` from the POST response, or
  `<PR URL>#pullrequestreview-<id>` built from its `id`; only its author sees it until submitted.
  Then one line per resolve or reopen: the action and the thread link.
- No → `**Draft review:** not pushed — <absolute path to review.json>`.
- Nothing to push → `**Draft review:** not pushed — no findings`.
