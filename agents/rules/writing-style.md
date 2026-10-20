# Writing style

Load before writing anything a person reads: answers, PR titles and bodies, commit messages,
review comments and replies, issues, and docs.

**Rule**: Lead with the point, give the reader the context they lack, and cut everything else.

- Bottom line first; detail follows only when it helps or is asked for.
- Write for someone who knows the project but not your session. The first time a component
  appears, say what it does, then use its real name.
- Use the names the code, the design, and the team already use. No coined labels and no labels
  from your own session — finding numbers from another report, draft names, private shorthand.
  An issue or PR number carries a few words on what it is.
- Every sentence says something checkable. A sentence that fits any system is filler; a step you
  do not know is established or marked unknown, never written past.
- No narration of how you got here — what you tried, what changed in review. State the result
  and, when it helps, why. Code comments follow [code-comments](code-comments.md) and carry no
  history either ("now", "no longer", "previously").
- Full sentences, not telegram fragments. No praise openers ("Great catch", "Thanks, …") and no
  stacked hedging.
- Never put production figures — balances, positions, debts, transfer amounts — in anything that
  leaves the workspace: PRs, commits, review comments, issues. Argue from the code path, or say
  "verified against production data" without the numbers.

Why: text is read by people who were not in the session. Jargon and narration make them
reconstruct what the writer already knew; missing context makes them guess.
