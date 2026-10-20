# Keep the agent index in sync

Load on request before changing anything in a directory that carries an `AGENTS.md` / `CLAUDE.md` — adding / renaming / removing paths, or changing build/run/test commands, env vars, or conventions.

**Rule**: When you change a directory that has its own agent index, update that index in the same commit so it still matches reality — not only `## Structure` on path changes, but also commands, env vars, conventions, and "where to make a change" pointers when those drift.

- Update the **nearest enclosing index**: the `AGENTS.md` / `CLAUDE.md` in the changed directory, or — if it has none — the closest ancestor directory that does. For a Rust change, that is the crate's own `AGENTS.md` under `crates/<crate>/` when it has one, otherwise [agents/AGENTS.md](../AGENTS.md); also update the repo-root index for any top-level path change.
- The index is `AGENTS.md` and/or `CLAUDE.md`. When `CLAUDE.md` is a symlink to `AGENTS.md` (the common setup here), edit that one file — both names resolve to it. Don't duplicate content across them.
- Before finishing a change, re-read the enclosing index and fix whatever no longer matches.

Triggers:

- New / renamed / removed top-level directory, or canonical top-level file (`CHANGELOG.md`, `Dockerfile`, etc).
- New / renamed / removed entry directly under a directory that has its own index.
- A documented command, env var, entry point, or convention in that index becomes wrong.

Not triggers:

- Nested files inside an already-listed dir whose contents the index does not enumerate.
- Build outputs / temp files.

Why: the index is what humans and agents land on at every level that has one. If `Structure`, commands, or conventions drift, readers look for files that don't exist, run commands that don't work, and miss what does.
