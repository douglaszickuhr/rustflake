#!/bin/sh
# Release notes from the Conventional Commits since the previous tag: feat, fix and perf ("!" = breaking).
# Usage: scripts/release-notes.sh v0.1.1
set -eu
tag=$1
prev=$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)
range=${prev:+$prev..}$tag
section() {
  lines=$(git log --no-merges --reverse --format=%s "$range" |
    sed -nE -e "s/^$2(\([^)]*\))?!: (.*)/- **Breaking:** \2/p" -e "s/^$2(\([^)]*\))?: (.*)/- \2/p")
  if [ -n "$lines" ]; then printf '## %s\n\n%s\n\n' "$1" "$lines"; fi
}
section Features feat
section Fixes fix
section Performance perf
[ -z "$prev" ] || printf '**Full changelog**: https://github.com/%s/compare/%s...%s\n' "${GITHUB_REPOSITORY:-douglaszickuhr/rustflake}" "$prev" "$tag"
