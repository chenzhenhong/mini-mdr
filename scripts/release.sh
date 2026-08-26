#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ -z "$version" ]; then
  echo "failed to read version from Cargo.toml" >&2
  exit 1
fi

tag="v${version}"
echo "version: ${version}"
echo "tag:     ${tag}"

if git rev-parse "$tag" >/dev/null 2>&1; then
  echo "tag ${tag} already exists" >&2
  exit 1
fi

git tag -a "$tag" -m "Release ${tag}"
git push origin "$tag"
echo "pushed ${tag}"
