#!/usr/bin/env bash
# Align the crate version with a release tag.
#
# Usage: scripts/set_version.sh <version>
#   where <version> is either "1.2.2" or "v1.2.2".
#
# Rewrites the `version = "..."` field in Cargo.toml and refreshes Cargo.lock.
# Used by the release workflow so the git tag is the single source of truth:
# the tag drives the published version, not a hand-edited Cargo.toml.

set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <version>" >&2
  exit 2
fi

# Accept both "v1.2.2" and "1.2.2".
version="${1#v}"

# Reject anything that is not a plain semver MAJOR.MINOR.PATCH.
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: '$version' is not a MAJOR.MINOR.PATCH version" >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/Cargo.toml"

# Rewrite only the first `version = "..."` line (the [package] one, which
# appears before any [dependencies] thanks to the file layout).
perl -0pi -e 's/^version = "[^"]*"/version = "'"$version"'"/m' "$manifest"

current="$(grep -m1 '^version = ' "$manifest" | sed -E 's/version = "([^"]*)"/\1/')"
if [ "$current" != "$version" ]; then
  echo "error: failed to set version (Cargo.toml still reads '$current')" >&2
  exit 1
fi

# Refresh Cargo.lock so the build stays --locked-clean.
(cd "$root" && cargo update -p mirage-tui --precise "$version" >/dev/null 2>&1 || cargo generate-lockfile)

echo "version set to $version"
