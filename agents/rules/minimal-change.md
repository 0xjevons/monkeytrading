# Minimal change

Load on request when designing an approach, reviewing your own draft, or editing any existing file.

**Rule**: Write the minimum code that solves the problem, and touch only what you must.

Minimum code (nothing speculative):

- KISS — no spaghetti; code should be practically self-documenting.
- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" / configurability that wasn't requested.
- No error handling for impossible scenarios.
- 200 lines could be 50 → rewrite.
- Self-check: would a senior engineer call this overcomplicated? If yes, simplify.

Minimum footprint (clean up only your own mess):

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor what isn't broken.
- Match existing style, even if you'd do it differently.
- Notice unrelated dead code → mention, don't delete.
- Remove imports / vars / functions YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

Test: every changed line traces directly to the user's request.

Why: speculative complexity and scope creep are liabilities — code you don't need still has to be
read, maintained, and debugged, and adjacent "improvements" pollute the diff and hide the real change.
