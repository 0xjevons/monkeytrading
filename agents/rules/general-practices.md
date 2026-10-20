# General practices

Load on request before any change.

**Rule**: Write clean, idiomatic, well-tested code, and never roll your own crypto.

- Never roll your own crypto.
- Prefer ripgrep (`rg`) over `grep` or other pattern tools unless explicitly asked to use `grep`.
- Prioritize clean, idiomatic code — use the language's features without overcomplicating.
- Write thorough, correct, well-documented unit and integration tests; the operator directs complex / end-to-end tests.

Keep changes minimal and clarify when unsure — see [minimal-change.md](minimal-change.md) and
[think-before-coding.md](think-before-coding.md).
