#!/usr/bin/env bash
#
# release.sh — build a stripped release binary and publish it as a GitHub
# release. The dotfiles install.sh downloads this asset into devcontainers
# (asset name must stay "diffobserver-x86_64-linux").
#
# The tag is v<version> from Cargo.toml. Workflow: bump `version` in Cargo.toml,
# commit, then run this. Requires cargo, gh (authenticated), and a clean tree.

set -euo pipefail

cd "$(dirname "$0")/.."   # repo root

ASSET="diffobserver-x86_64-linux"
version="$(grep -m1 '^version' Cargo.toml | sed -E 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/')"
tag="v${version}"

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty — commit before releasing." >&2
    exit 1
fi
if gh release view "$tag" >/dev/null 2>&1; then
    echo "error: release $tag already exists — bump 'version' in Cargo.toml first." >&2
    exit 1
fi

echo "==> Building release binary..."
cargo build --release

tmp="$(mktemp -d)"
cp target/release/diffobserver "$tmp/$ASSET"
strip "$tmp/$ASSET"

echo "==> Pushing HEAD and creating release $tag..."
git push
gh release create "$tag" "$tmp/$ASSET" \
    --target "$(git rev-parse HEAD)" \
    --title "$tag" \
    --generate-notes

rm -rf "$tmp"
echo "==> Released: $(gh release view "$tag" --json url --jq .url)"
