#!/usr/bin/env bash
# Check that a release is ready to publish, before anything is built:
#
#   tools/check_release.sh v2.1.0          (the release workflow passes the tag)
#
#   - Cargo.toml's version is the tag's version;
#   - CHANGELOG.md has a dated heading for it, "## [2.1.0] - 2026-10-28",
#     not "- unreleased", with a real date;
#   - that heading is the newest versioned one (only "## [Unreleased]" may
#     come before it).
#
# Exits 1 with a message naming what to fix.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANGELOG="${CHANGELOG:-$ROOT/CHANGELOG.md}"
CARGO="${CARGO_TOML:-$ROOT/Cargo.toml}"

[ $# -eq 1 ] || { echo "usage: $0 vX.Y.Z" >&2; exit 2; }
version="${1#refs/tags/}"
version="${version#v}"
fail=0
err() { printf 'release check: %s\n' "$*" >&2; fail=1; }

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { err "tag '$1' is not vX.Y.Z"; exit 1; }

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO" | head -1)"
[ "$cargo_version" = "$version" ] ||
  err "Cargo.toml says version $cargo_version, the tag says $version"

esc="${version//./\\.}"
heading="$(grep -E "^## \[$esc\]" "$CHANGELOG" | head -1 || true)"
if [ -z "$heading" ]; then
  err "CHANGELOG.md has no '## [$version] - YYYY-MM-DD' heading"
else
  date="$(sed -nE 's/^## \[[^]]+\] - ([0-9]{4}-[0-9]{2}-[0-9]{2})[[:space:]]*$/\1/p' <<<"$heading")"
  if [ -z "$date" ]; then
    err "CHANGELOG.md heading '$heading' has no date: write '## [$version] - $(date -u +%F)'"
  elif ! date -u -d "$date" +%F >/dev/null 2>&1 || [ "$(date -u -d "$date" +%F)" != "$date" ]; then
    err "CHANGELOG.md heading '$heading': $date is not a real date"
  fi
  first="$(grep -E '^## \[' "$CHANGELOG" | grep -viE '^## \[unreleased\]' | head -1)"
  [ "$first" = "$heading" ] ||
    err "'$heading' is not the newest release in CHANGELOG.md (the first is '$first')"
fi

[ "$fail" -eq 0 ] && echo "release check: $version ready (Cargo.toml and CHANGELOG.md agree, dated $date)"
exit "$fail"
