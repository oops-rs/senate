#!/usr/bin/env bash
# Builds the Town page from the top-level town/ package and copies it into
# the app's bundle folder, Senate/Web/town/ (committed, so Xcode never needs
# Node). Town is its own package: the app only consumes its built page.
#
# Usage, from anywhere: apps/macos/scripts/sync-town.sh
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$root/town"
[[ -d node_modules ]] || npm ci
npm run --silent build
dest="$root/apps/macos/Senate/Web/town"
rm -rf "$dest"
cp -R dist "$dest"
echo "synced town/dist → apps/macos/Senate/Web/town"
