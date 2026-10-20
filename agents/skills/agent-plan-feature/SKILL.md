---
name: agent-plan-feature
description: >
  Use when turning a spec or project brief into GitHub issues, or re-planning an existing project —
  "break this spec into issues", "plan the build", "organise the milestones", "which features are
  ready". Groups the work into complete features that each ship as one PR, run end to end on a dev
  node, and can be promoted to prod; gates each feature on the decisions it needs; writes issues
  that cite their sources. The user revises and approves the plan before any issue write.
---

# agent-plan-feature

Turn a spec into a short list of features, each a complete, cohesive change that ships as one PR.
Implementation is now the cheap part; human review time, context, and wrong guesses are the
expensive ones. Settle the shape of the work here, so each feature is reviewed once, tested in dev,
and promoted to prod. Build a planned feature with
[agent-build-feature](../agent-build-feature/SKILL.md).

## What a feature is

- **Vertical.** One capability, end to end: the protocol rules in `engine-core`, the node wiring,
  adapters, API, config, and contract changes it needs to run. A layer on its own (types, a state
  machine, a schema) is not a feature.
- **Backend and frontend ship separately.** A backend feature is complete when it runs end to end on
  a dev node through its API, an operator command, or a scripted check. The app work that uses it
  is its own feature, in its own PR, delivered after it and depending on it.
- **Testable in dev.** A scenario someone can run in dev with an observable result. Until a hosted
  dev environment exists, dev is a node on `config/dev.toml` (mock TEE, custody, and feeds), plus a
  local or test chain for contract work. When dev cannot run it (a chain, venue, or fixture is
  missing), how dev will run it is a decision that blocks the feature.
- **Promotable.** It can reach prod on its own, dark behind config if it must, and leaves no partial
  state for a later feature to repair. Its promotion notes name any feature that must be live in
  prod before this one is enabled for customers.
- **Reviewable in one round.** One idea a reviewer can hold at once. The first signal is the
  feature's shape: if its dev scenario could be cut into two that each end in a result someone can
  check through an API, look for a seam there, a split into features that each keep their own API
  and dev scenario. Common seams:
  - request signing apart from the flows it protects: EIP-712 verification and nonces before
    withdrawals and orders depend on them;
  - a write flow apart from the API that reads it back: crediting a vault deposit, proven in dev by
    chain state and the account balance, before its history API. The write feature's promotion
    notes keep it off for customers until the read feature is live;
  - a narrower trigger, one direction, or one step of a flow.

  A seam is a reason to look, not to split: each extra feature costs its own dev run and
  promotion, so a small feature with a write and a read can stay whole. Size is the backstop,
  counting production code and tests, not docs, lockfiles, generated files, or prod's copy of dev
  infra. Past about 5k lines, look for a seam, and when there is none, record why. Past about 10k, a
  PR is rarely one idea: split unless the user agrees there is no seam. Never split by layer.
- **Thin first.** A new component starts with a walking skeleton: the narrowest path through every
  layer, running in dev. Later features widen it. Shared runtime code (an adapter contract, a
  signing helper, a log reader) belongs to the first feature that needs it, not to a feature of its
  own.

## Readiness

A feature is **Ready** only when every decision its design depends on is made and recorded where
the team records decisions: the spec, or the decision issue that owns it. When the two disagree,
the newer record wins, and the plan lists each one it followed. While a decision is open, the whole
feature is **Not ready**: never build its defined parts early. They are quick to write and slow to
review, and the open decision can still reshape them. GitHub shows readiness with labels:
`ready-to-build`, or `pending-design` with the open decision issues linked as blockers.

- **What gates.**
  - A choice about behaviour that users, money, or the house's equity holders depend on (what
    counts as settled, whether a fee exists, how house equity is issued) blocks the build, even when
    its value later lands in `ProtocolParams` or a `MarketSpec`.
  - A number the design reads from config (a threshold, a cap) blocks promotion, not the build:
    name it under the feature's promotion with its owning issue.
  - An internal interface or schema that only the feature uses is settled in its PR review.
- **Decisions and evidence** (vendor tests, cost models, legal text) are their own issues, titled
  `Decision: …` or `Evidence: …`. Each has a proposed owner, sits in the milestone of the first
  feature it blocks, and blocks the features that need it through GitHub's blocked-by relationship
  (plus a `Blocked by #<n>` line in the blocked issue's body).
- **Platform prerequisites** (chain access, enclave hardware and attestation, signing keys, venue
  data feeds, deployment, monitoring)
  belong to the first feature that needs them, or form a feature of their own, with its own dev
  scenario, when several share them. They are never left for the end.
- **Conditional features,** which exist only if a decision goes one way, are listed in the plan as
  conditional and created when the decision lands.

## Issue structure

- **One issue per feature.** Its description holds, each item citing its source (spec link and §,
  or decision issue):
  - **Readiness:** the `ready-to-build` or `pending-design` label, with each open decision issue
    linked as a blocker.
  - **Acceptance criteria.** A criterion with no source is an assumption and says so.
  - **Dev scenario:** the steps and the result that prove the feature works in dev.
  - **Promotion:** what prod needs — config, secrets, genesis or parameter changes, contract
    deployments, ordering, rollback, and the features that must be live first.
  - **Size:** the rough size of its PR and, past about 5k lines, why it has no seam.
- **Implementation steps are a checklist in the feature issue,** not issues of their own. Separate
  issues for parts of one feature become separate PRs and separate reviews.
- **Milestones are shippable features in delivery order,** not technical layers. Each milestone ends
  with something running in dev.
- The feature's builder owns the feature issue; decision owners own decision issues.

## Re-planning an existing project

- Read every issue and its comments first; decisions often live there rather than in the spec.
- Give each issue one disposition: **host** (rewritten as a feature issue), **merged** (closed as a
  duplicate, its criteria copied into the host), **placed** (done work a feature consumes),
  **decision**, **evidence**, or **drop** with a reason. Rewrite an issue rather than closing it
  and creating another, so history and links survive.
- Code already merged as a layer stays. Place it in the first feature that consumes it, and check it
  against that feature's decisions as they land.

## Process

1. Read the spec end to end, then the services, libraries, and infra the features would reuse, so
   platform gaps show up now rather than at deploy time.
2. Draft the feature list: title, the capability in one line, dev scenario, decisions needed,
   dependencies, and a rough size of its PR. Expect features, not tasks: a large spec yields tens
   of features, not one issue per module. Until the issues exist, number features in delivery
   order (F1, F2, …) and say the numbers become issue numbers on creation.
3. Walk each feature for what must exist before it runs in dev and prod — feeds, chain access,
   the enclave and its key, gas, config, secrets, deployment, monitoring, operator recovery, user
   eligibility. A need with no home is a gap; add it to a feature.
4. For a large spec, have the draft reviewed independently through
   [ask-agent](../ask-agent/SKILL.md), by the other model family from the one that drafted it, with
   read-only access. Give the reviewer the spec, the plan, the repo, and the decisions the plan
   takes from outside the spec. Ask for gaps, features that are not vertical, large features with
   a seam the plan missed, and Ready verdicts that rest on an open decision. Apply each finding or
   record in the plan why not; re-review only if the feature list changes.
5. Show the user a one-screen summary — features in order with readiness, the decisions on the
   critical path, the milestones — with the full plan and the exact issue text in files. Revise it
   with them until they approve, then write the approved plan to GitHub (issues, labels,
   milestones) in one batch. agent-build-feature builds from that approved plan, so afterwards
   change the issues and the plan together.

## Rules

- Never create, edit, or close a GitHub issue, label, or milestone without the user's explicit
  approval.
- Never mark a feature Ready so work can start sooner.
- Re-plan when a decision lands that changes a Ready feature; say which features it moved.
- Writing follows [writing-style](../../rules/writing-style.md).
