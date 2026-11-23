#!/usr/bin/env sh
# Build the root helper separately and stage the exact release checksum.
set -eu

if [ "$#" -ne 1 ]; then
    printf 'usage: %s OUTPUT_DIRECTORY\n' "$0" >&2
    exit 2
fi
mkdir -p "$1"
out=$(cd "$1" && pwd)
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
build=${CARGO_TARGET_DIR:-target}
cargo build --locked --release --target-dir "$build" -p senate-remote-admin --bin senate-remote-admin
source=$(cd "$build/release" && pwd)/senate-remote-admin
if [ "$source" != "$out/senate-remote-admin" ]; then
    cp "$source" "$out/senate-remote-admin"
fi
if [ -n "${SENATE_REMOTE_ADMIN_SIGN_IDENTITY:-}" ] &&
   [ "$SENATE_REMOTE_ADMIN_SIGN_IDENTITY" != "-" ]; then
    /usr/bin/codesign --force --options runtime --identifier dev.senate.remote-admin \
        --sign "$SENATE_REMOTE_ADMIN_SIGN_IDENTITY" "${SENATE_REMOTE_ADMIN_TIMESTAMP:---timestamp=none}" \
        "$out/senate-remote-admin"
    /usr/bin/codesign --verify --strict "$out/senate-remote-admin"
fi
cd "$out"
if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 senate-remote-admin > senate-remote-admin.sha256
else
    sha256sum senate-remote-admin > senate-remote-admin.sha256
fi
cat senate-remote-admin.sha256
