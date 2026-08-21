#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <version>  (e.g. $0 0.2.0)" >&2
    exit 1
fi

VERSION="$1"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: version must look like x.y.z (e.g. 0.2.0)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is not clean, commit or stash your changes first" >&2
    exit 1
fi

if git rev-parse "v${VERSION}" >/dev/null 2>&1; then
    echo "error: tag v${VERSION} already exists" >&2
    exit 1
fi

echo "Bumping version to ${VERSION}..."

sed -i -E "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml
sed -i -E "s/(\"version\": \")[^\"]*(\")/\1${VERSION}\2/" ui/package.json

if [ -f Cargo.lock ]; then
    cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace
fi

if command -v npm >/dev/null 2>&1 && [ -f ui/package-lock.json ]; then
    (cd ui && npm install --package-lock-only --silent)
fi

git add Cargo.toml ui/package.json Cargo.lock ui/package-lock.json

git commit -m "release: v${VERSION}"
git tag -a "v${VERSION}" -m "v${VERSION}"

git push origin HEAD
git push origin "v${VERSION}"

echo "Released v${VERSION}"
