#!/usr/bin/env bash
# Copy the reviewed workflow files over `.github/workflows/`.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
for name in ci.yml release-android.yml release-desktop.yml publish-release.yml; do
  test -f "$here/$name" || { echo "missing $here/$name" >&2; exit 1; }
  cp "$here/$name" "$root/.github/workflows/$name"
  echo "applied .github/workflows/$name"
done
echo "Now commit the workflow changes with a token that has the workflow scope."
