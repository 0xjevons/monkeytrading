# agent-review-pr: review and re-review

Load [no-guessing-current-state](../../../rules/no-guessing-current-state.md). Use `gh` with the
identity from `gh api user --jq .login` for every read and write; never print or persist a token.

A first review and a re-review run the same steps in this order. A run is a **re-review** when
that identity already has a submitted review or owns a review-thread root comment on the PR; it
adds step 6, which rechecks those earlier threads, and everything else stays the same.

## 1. Isolate the code

Record the head and the base state:

```bash
gh pr view "$PR" --json headRefName,headRefOid,baseRefName,mergeable,mergeStateStatus
```

Inspect `git worktree list --porcelain` and reuse a clean worktree whose `HEAD` equals the head
SHA. Otherwise run [agent-create-worktree](../../agent-create-worktree/SKILL.md) in review mode:
detached at the recorded SHA, named after the PR's original head branch, and moving a clean review
tree left from an earlier head instead of recreating it. The review request is the approval
`agent-code-review` asks for before fetching that SHA. Never reset, clean, or overwrite an existing
worktree.

The worktree is read-only. Keep it until the push question is answered; remove it afterwards only
if this run created it.

Check the base: when the head does not contain the base tip and the base has since changed files
the PR touches, say so in the report and check each finding against the base as well — a finding
judged against an outdated base can be confidently wrong.

## 2. Run the review

Load [agent-code-review](../../agent-code-review/SKILL.md) and run it in PR scope over the whole
current PR diff, from the checkout this skill started in, not from the review worktree; the review
worktree is its checkout at the recorded head SHA. Its run directory in that checkout,
`agents/.processes/reviews/<YYYY-MM-DD-HHMM>-<kebab-slug>/`, is `<run-dir>` below. Run the mode
the request selected and pass requested review levels through unchanged. Skip its completion
report; step 10 prints this skill's report instead. The review stays blind to the PR's existing
comments, so earlier reviewers do not anchor it. Give it, and in multi-agent mode every reviewer
prompt, this condition:

> Report only problems this PR introduces, changes, or worsens. Do not report pre-existing issues
> in untouched code, refactors, speculative hardening, or nice-to-haves unless this PR's change
> concretely breaks or depends on them.

Its `.pr.json` and `.pr.diff` are the immutable scope; every comment anchor comes from `.pr.diff`.
Save the report inputs next to them:

```bash
gh pr view "$PR" --json number,title,body,url,author,headRefName,additions,deletions,changedFiles \
  > <run-dir>/.pr-summary.json
gh pr checks "$PR" --json name,workflow,state,bucket,link > <run-dir>/.pr-checks.json
```

## 3. Filter

Read `<run-dir>/code-review-final.md` and apply the skill's keep rules:

- Drop a finding outside the PR's scope.
- Recheck every `Low`-confidence finding in detail against the code at the head. When the code
  confirms it, raise its `Confidence` line in `code-review-final.md` to what the evidence supports
  and note the evidence in `<run-dir>/draft-filter.md`; when the code disproves it or cannot settle
  it, drop it. No `Low` finding goes further.

Record each dropped finding's title and reason in `<run-dir>/draft-filter.md`. Never re-run the
review, reword a finding, or change its severity.

## 4. Read the discussion

Only now, with the findings final, fetch every page of the PR's reviews, review threads, and
top-level comments:

```bash
gh api --paginate "repos/{owner}/{repo}/pulls/$PR/reviews" > <run-dir>/.reviews.json
gh api --paginate "repos/{owner}/{repo}/issues/$PR/comments" > <run-dir>/.issue-comments.json
gh api graphql --paginate -F owner='{owner}' -F repo='{repo}' -F pr="$PR" -f query='
query($owner: String!, $repo: String!, $pr: Int!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $endCursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line
          comments(first: 100) {
            pageInfo { hasNextPage }
            nodes { databaseId author { login } body url createdAt }
          }
        }
      }
    }
  }
}' > <run-dir>/.threads.json
```

A thread whose `comments.pageInfo.hasNextPage` is true has more comments than the first page:
fetch the rest before reading it, so its last comment really is the last one.

```bash
gh api graphql --paginate -F id="$THREAD_ID" -f query='
query($id: ID!, $endCursor: String) {
  node(id: $id) { ... on PullRequestReviewThread {
    comments(first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes { databaseId author { login } body url createdAt }
    }
  } }
}'
```

## 5. Match findings to the discussion

For each kept finding, look for a thread or comment — anyone's — that already raises the same
concern:

- It adds nothing the thread lacks → drop it (`draft-filter.md`: "already raised in <url>").
- It adds something — a concrete scenario, evidence, a fix direction → draft it as a reply in that
  thread; it stays out of the new comments. The reply carries only what the finding adds, in one or
  two plain sentences with no labels, as step 6 describes.
- Otherwise it stays a new inline comment.

## 6. Recheck earlier threads (re-review only)

Take the threads whose root comment belongs to the identity. The `commit_id` of its newest
submitted review is the last reviewed SHA; read the delta from it to the current head as well as
the whole PR diff (only the whole diff when `git merge-base --is-ancestor` says that SHA is gone).

Verify every earlier thread's concern against the current code and tests. An outdated anchor does
not make a concern fixed, and a moved line is still the same thread.

| Current result | Action |
| --- | --- |
| Fixed | `resolve` the thread. |
| Declined by the author with a reason | State whether you agree with the reason. Agree → `resolve`; the report says the reason was accepted. Disagree → `reply` answering that reason, never repeating the original request. The user's call → `needs a decision`: listed in the report, nothing posted. |
| Still there, or partly fixed | `reply` with the concrete change still required. |
| Was resolved, now broken again | `reopen + reply`. |
| Already asked, and no commit since | `outstanding`: no new reply. |

Record the actions from steps 5 and 6 in `<run-dir>/thread-actions.md`. Replies are one or two
plain sentences following [writing-style](../../../rules/writing-style.md), with no severity tag or
heading. Resolve or reopen only the identity's own threads.

## 7. Build the payload

The review payload carries the new comments; the replies attach to the same review in step 11.
With no new comment, `review.json` holds an empty `comments` list. Each new comment's body is its
finding's `Severity`, `Complexity`, `Confidence`, `Issue`, and `Suggested fix` values from
`code-review-final.md`, copied unchanged into
[templates/inline-comment.md](../templates/inline-comment.md). Nothing else goes in: no title,
scenario, reporters, validators, file, or signature.

Anchor it at the first `File(s)` location that is a changed or context line in `.pr.diff`: `RIGHT`
for added or context lines, `LEFT` for deletions, and `start_line` / `line` (plus `start_side`
when needed) for a `<start>-<end>` span. Never invent an anchor.

The review `body` is [templates/review-body.md](../templates/review-body.md) filled in:

- `Overview` — what this review found, never what the PR does, following
  [agent-code-review's `Overview` rule](../../agent-code-review/SKILL.md#completion-report) but
  counting only the comments and threads this run posts. It never repeats an inline comment's or a
  reply's text.
- `Outside the diff` — optional. One `LOCATION` line and one `FINDING` block per finding with no
  anchor: its `File(s)` value, or `PR description` / the cross-PR link, then its comment file.
  Repeat the pair for each such finding; drop the heading and both placeholders when there is none.
- `Verdict` — last: `APPROVE` or `REQUEST CHANGES` as step 10 decides it, and nothing else; the
  `Overview` is its reason.

Never type JSON strings by hand: a hand-written body posts `\n` as literal text. For finding `<n>`,
copy the template to `<run-dir>/comments/<n>.md` and replace each placeholder with the finding's
value, keeping the five lines, their order, and their labels. Then let `jq --rawfile` encode the
files:

```bash
jq -n --arg commit "$HEAD_SHA" '{commit_id: $commit, body: "", comments: []}' \
  > <run-dir>/review.json

# One finding on a single line; for a span add --argjson start_line <start> and
# `start_line: $start_line` (plus start_side) to the object.
jq --arg path "<path>" --argjson line <line> --arg side RIGHT \
  --rawfile body <run-dir>/comments/<n>.md \
  '.comments += [{path: $path, line: $line, side: $side, body: $body}]' \
  <run-dir>/review.json > <run-dir>/review.json.tmp \
  && mv <run-dir>/review.json.tmp <run-dir>/review.json

# The body: <run-dir>/review-body.md, the review-body template filled as described above.
jq --rawfile body <run-dir>/review-body.md '.body = $body' <run-dir>/review.json \
  > <run-dir>/review.json.tmp && mv <run-dir>/review.json.tmp <run-dir>/review.json
```

## 8. Validate

The check fails when:

- an inline comment does not start with the template's first line, or has fewer than five lines (a
  hand-escaped body collapses to one);
- the review body does not open with the `Overview` line (at most 300 characters) and a blank
  line, does not end with `**Verdict:** APPROVE` or `**Verdict:** REQUEST CHANGES`, holds a
  finding without the `Outside the diff` heading, or holds a finding collapsed to fewer than five
  lines;
- any body still holds a template placeholder.

Text a finding legitimately quotes, such as `\n` or `{{name}}`, passes. On failure, fix the source
file and rebuild; never patch the JSON.

```bash
jq -e '
  (.comments | all(.body | startswith("- **Severity:** ")
      and (split("\n") | map(select(length > 0)) | length) >= 5))
  and (.body | split("\n") as $l
      | ($l[0] | startswith("**Overview:** ") and length <= 300)
      and $l[1] == ""
      and ($l | map(select(length > 0)) | last
           | test("^\\*\\*Verdict:\\*\\* (APPROVE|REQUEST CHANGES)$"))
      and ((contains("- **Severity:** ") | not) or contains("\n**Outside the diff:**\n"))
      and (($l | map(select(startswith("- **"))) | length)
           >= 5 * ([match("- \\*\\*Severity:\\*\\* "; "g")] | length)))
  and ([.body, .comments[].body]
      | all(test("\\{\\{(OVERVIEW|VERDICT|LOCATION|FINDING|SEVERITY|COMPLEXITY|CONFIDENCE|ISSUE|SUGGESTED_FIX)\\}\\}")
            | not))' \
  <run-dir>/review.json
```

## 9. Re-check live state

Re-read the PR head, your pending-review state, and every thread the run acts on — others may have
posted meanwhile. A head change makes the diff and line numbers stale: stop and report it; the next
run starts from the new head. A changed thread means re-running steps 5 and 6 for it.

## 10. Report

Print [templates/report.md](../templates/report.md) filled in: replace every `{{…}}`, and keep
every label and its order. `Why`, `What`, `Fit to issue`, `Risk`, `Tests`, `Diff`, `Loaded`, and
`Not verified` follow
[agent-code-review's completion-report rules](../../agent-code-review/SKILL.md#completion-report).
Then:

- **Header:** PR number, title, every issue in the branch name or body (or `no issue`), and the
  author's login.
- **Verdict:** `REQUEST CHANGES` when a kept finding — whether it posts as a new comment or as a
  reply — or an earlier thread left open (`reply`, `reopen + reply`, or `outstanding`) is Critical,
  High, or Medium; an earlier thread goes by the `Severity` line of its root comment, or by the
  [severity guide](../../agent-code-review/references/severity.md) when it has none. Otherwise
  `APPROVE`. The `Overview` gives the reason; the verdict carries none of its own.
- **Draft comments:** one item per new inline comment in posting order. Its sub-items are exactly
  the lines the comment will post; `none` when there are none.
- **Review body:** the body exactly as step 7 wrote it into `review.json` — `Overview`, the
  optional `Outside the diff` block, and `Verdict`. With the draft comments, it is the push preview.
- **Existing threads:** one item per thread action from steps 5 and 6 — its action, link, and
  concern, plus the exact reply when there is one; a `needs a decision` item says what the user
  must decide and posts nothing. `none` when there are none.
- **Dropped:** one item per `draft-filter.md` entry with its reason, or `none`.
- **Base:** `up to date`, or the base-file overlap from step 1.
- **Head:** the recorded head SHA.
- **CI:** counts per bucket from `.pr-checks.json`, or `not run` when there are no checks.
- **Mode:** `current-agent`, or `multi-agent` with the review levels and reviewer models.
- **Artifacts:** the absolute `<run-dir>` path.

The report is user-facing only: of it, only the review body's `Overview` and `Verdict` reach
GitHub; never put the rest in `review.json` or a comment.

## 11. Ask, then post

With no new comment and no thread action, leave out the push question and post nothing.
Otherwise the report ends with **Push the draft comments?**; a yes covers the review body, the new
comments, and the thread actions listed in the report. On no, post nothing.

On yes, re-run step 9, then post in this order:

1. The review, without an `event` field — that keeps it PENDING. `APPROVE`, `REQUEST_CHANGES`,
   `COMMENT`, and `gh pr review` submit it and are forbidden.

   ```bash
   gh api --method POST -H "Accept: application/vnd.github+json" \
     "repos/{owner}/{repo}/pulls/$PR/reviews" --input <run-dir>/review.json \
     > <run-dir>/.posted-review.json
   jq -r '.html_url // empty, .id, .node_id' <run-dir>/.posted-review.json
   ```

2. Every reply, staged in that pending review with its `node_id`, verbatim:

   ```bash
   gh api graphql -F review="$REVIEW_NODE_ID" -F thread="$THREAD_ID" -f body="$REPLY" -f query='
   mutation($review: ID!, $thread: ID!, $body: String!) {
     addPullRequestReviewThreadReply(input: {
       pullRequestReviewId: $review, pullRequestReviewThreadId: $thread, body: $body
     }) { comment { url } }
   }'
   ```

3. Resolves and reopens, on the identity's own threads:

   ```bash
   gh api graphql -F id="$THREAD_ID" \
     -f query='mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { id } } }'
   gh api graphql -F id="$THREAD_ID" \
     -f query='mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { id } } }'
   ```

## 12. Finish

Remove only a worktree this run created, then print the skill's completion line.
