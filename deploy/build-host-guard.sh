#!/bin/sh
# Build the small Linux launcher without privilege. The installer checks its digest.
set -eu
if [ "$(uname -s)" != Linux ]; then
    echo "Build the cgroup launcher on Linux." >&2
    exit 1
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
output=${1:-target/host-guard}
mkdir -p "$output"
cc -std=c11 -O2 -Wall -Wextra -Werror -fstack-protector-strong -D_FORTIFY_SOURCE=3 \
    -Wl,-z,relro,-z,now "$script_dir/senate-host-guard.c" -o "$output/senate-host-guard"
(cd "$output" && sha256sum senate-host-guard > senate-host-guard.sha256)
