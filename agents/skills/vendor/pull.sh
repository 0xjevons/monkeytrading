#!/usr/bin/env bash
# Pull vendored skill packs into skills/vendor/<pack>/ from the commit-pinned
# tarball URLs in vendor.manifest. Packs are NOT committed (see .gitignore); this
# is the only way to materialize them locally.
#
# Usage:
#   ./pull.sh                 # pull every pack that is missing
#   ./pull.sh <pack>          # pull one pack if missing
#   ./pull.sh --force [<pack>] # re-pull even if present (refresh)
#
# Idempotent: a pack that already exists is skipped unless --force is given.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="$here/vendor.manifest"

force=0
if [[ "${1:-}" == "--force" ]]; then
  force=1
  shift
fi
only="${1:-}"

[[ -f "$manifest" ]] || { echo "missing manifest: $manifest" >&2; exit 1; }

pulled=0
while IFS=$'\t' read -r pack url subtree; do
  [[ -z "${pack:-}" || "$pack" == \#* ]] && continue
  [[ -n "$only" && "$only" != "$pack" ]] && continue

  dest="$here/$pack"
  if [[ -d "$dest" && $force -eq 0 ]]; then
    echo "skip  $pack (present)"
    continue
  fi

  echo "pull  $pack <- $url"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "$url" | tar -xz -C "$tmp"

  root="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -1)"
  src="$root"
  [[ "$subtree" != "." ]] && src="$root/$subtree"
  if [[ ! -d "$src" ]]; then
    echo "ERROR: subtree '$subtree' not found in $pack tarball" >&2
    rm -rf "$tmp"; trap - EXIT; exit 1
  fi

  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$src"/. "$dest"/
  rm -rf "$tmp"; trap - EXIT
  pulled=$((pulled + 1))
done < "$manifest"

echo "done (${pulled} pulled)"
