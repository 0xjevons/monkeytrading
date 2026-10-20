#!/usr/bin/env bash
# Fail if a committed reference log was changed, renamed, or deleted since a base ref (default
# origin/main). A reference log is written once per protocol version and never regenerated; adding
# a new one is the only allowed change.

set -euo pipefail

cd "$(dirname "$0")/.."

base=${1:-origin/main}
dir=crates/core/tests/reference

# Lower-case `a` excludes additions: every other status is a change to a committed log.
changed=$(git diff --name-status --diff-filter=a "$base"...HEAD -- "$dir")
if [[ -n "$changed" ]]; then
  echo "Committed reference logs must never change; a rule change ships as a new protocol version with its own log:"
  echo "$changed"
  exit 1
fi
echo "Committed reference logs are unchanged since $base."
