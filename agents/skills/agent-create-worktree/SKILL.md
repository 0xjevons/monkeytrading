---
name: agent-create-worktree
description: >
  Use when starting isolated work on a repo in this workspace and you want it on a separate git
  worktree instead of the main checkout — "create a worktree", "spin up a working tree", "give me
  a worktree for <branch>", "isolate this on its own tree". Puts the worktree under the repo's
  sibling `<repo>-worktrees/` directory, named after the branch, and checks out or creates the
  branch — or, to review a PR, a detached tree at its head.
---

# agent-create-worktree

Create a git worktree in a predictable place with a predictable name, so several branches of one
repo can be checked out side by side without disturbing the main checkout. Every worktree for a
repo lives under one sibling directory, `<workspace>/<repo>-worktrees/` (for example
`engine-worktrees/`), never inside the repo.

Not needed when you only edit the branch you are already on — work in place.

## Resolve the target

Decide the mode, branch, and directory name before touching git. The directory name is the branch
with every `/` replaced by `-`, so the prefix is kept and nothing nests.

1. **Review a PR** (read-only, as `agent-review-pr` does) → a detached tree at the PR's head SHA,
   fetched from `pull/<n>/head` so fork PRs work too. Name it after the PR's original head branch
   plus `-review`, so it never collides with an editing tree of the same branch. A clean, detached
   review tree already at that path is moved to the new head instead of recreated.
2. **Edit a PR** → attach to the PR's head branch with its original name; never rename or
   re-prefix it, and never create it. Stop on a fork PR (`gh pr view "$PR" --json
   isCrossRepository`): its branch is not on `origin`. A local copy must match the PR head:
   fast-forward it with `git fetch origin "$BRANCH:$BRANCH"`, and stop if git refuses (the branch
   is checked out elsewhere or has local commits).
3. **Branch given** → use it as named.
4. **Topic given** → form `{gh user}/<slug>[-gh<issue>]` per
   [git-workflow](../../rules/git-workflow.md); ask whether there is an issue when it is unknown.
5. **Neither** → stop and ask what to name it. Do not invent a branch.

## Create it

```bash
# Main checkout, even when run from inside another worktree.
REPO_ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
WT_ROOT="$(dirname "$REPO_ROOT")/$(basename "$REPO_ROOT")-worktrees"
mkdir -p "$WT_ROOT"
git -C "$REPO_ROOT" fetch origin --quiet
```

Branch modes (2–4):

```bash
BRANCH="$(gh api user --jq .login)/size-spread-tiers-gh12"          # resolved above
PR=""                                                                 # the PR number in mode 2
WT_PATH="$WT_ROOT/${BRANCH//\//-}"
if [ -e "$WT_PATH" ]; then
  echo "worktree path exists: $WT_PATH — stop"
elif git -C "$REPO_ROOT" show-ref --verify --quiet "refs/heads/$BRANCH" \
     || git -C "$REPO_ROOT" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null 2>&1; then
  git -C "$REPO_ROOT" worktree add "$WT_PATH" "$BRANCH"                          # existing branch
elif [ -n "$PR" ]; then
  echo "PR head branch not found: $BRANCH — stop"                                # mode 2 never creates
else
  DEFAULT="$(git -C "$REPO_ROOT" symbolic-ref --quiet --short refs/remotes/origin/HEAD \
    || echo "origin/$(gh repo view --json defaultBranchRef -q .defaultBranchRef.name)")"
  git -C "$REPO_ROOT" worktree add -b "$BRANCH" "$WT_PATH" "${BASE:-$DEFAULT}"   # new branch
  rm -f "$REPO_ROOT/agents/.processes/fixes/$BRANCH.md"                          # no stale fix notes (git-workflow)
fi
```

Review mode (1):

```bash
PR=42
read -r HEAD_BRANCH HEAD_SHA < <(gh pr view "$PR" --json headRefName,headRefOid \
  --jq '"\(.headRefName) \(.headRefOid)"')   # or the SHA the caller already recorded
WT_PATH="$WT_ROOT/${HEAD_BRANCH//\//-}-review"
git -C "$REPO_ROOT" fetch origin "pull/$PR/head" --quiet
if [ "$(git -C "$REPO_ROOT" rev-parse FETCH_HEAD)" != "$HEAD_SHA" ]; then
  echo "PR head moved since it was recorded — stop"
elif [ ! -e "$WT_PATH" ]; then
  git -C "$REPO_ROOT" worktree add --detach "$WT_PATH" "$HEAD_SHA"
elif [ "$(git -C "$WT_PATH" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" \
       = "$REPO_ROOT/.git" ] \
     && ! git -C "$WT_PATH" symbolic-ref -q HEAD >/dev/null \
     && [ -z "$(git -C "$WT_PATH" status --porcelain)" ]; then
  git -C "$WT_PATH" checkout --quiet --detach "$HEAD_SHA"   # reuse the clean review tree
else
  echo "worktree path exists and is not a clean detached review tree: $WT_PATH — stop"
fi
```

The repo has no submodules, so a new worktree needs nothing initialised. Each worktree builds
into its own `target/`, so its first `cargo` command is a full compile; run it in the background.

Report the path and how to enter it: `cd "$WT_PATH"`.

## Clean up

```bash
git -C "$REPO_ROOT" worktree remove "$WT_PATH"   # add --force only if you accept losing changes
git -C "$REPO_ROOT" worktree prune
```

## Guardrails

- Worktrees go in the sibling `<repo>-worktrees/`, never inside the repo. Build the path from the
  main checkout as above; never pass a relative path to `git worktree add`.
- A PR's worktree keeps the PR's head branch name: attached for editing, in the directory name for
  review.
- Never overwrite an existing worktree path, and never force a branch that is checked out
  elsewhere. The one reuse is moving a clean, detached review tree to the PR's new head.
- Base a new branch on the repo's default branch unless told otherwise.
- This skill only creates or removes the tree, and clears a new branch's leftover fix notes. It
  does not commit, push, or open a PR — that is
  [agent-create-pr](../agent-create-pr/SKILL.md).
