#!/bin/sh
# Sets the release version everywhere it is written, or checks that every
# place agrees.
#
#   deploy/bump-version.sh 0.14.0     write 0.14.0 in every place, then
#                                     refresh both lock files
#   deploy/bump-version.sh --check    say the version; fail if a place differs
#
# Options:
#   --no-lock      do not refresh the lock files
#   --root <dir>   work on <dir> instead of this repository
#
# The places (docs/release.md says what comes after):
#   Cargo.toml                the workspace version, and the version of each
#                             workspace crate in [workspace.dependencies]
#   shim/senate/Cargo.toml    the package version, and senate-bridge's
#   apps/macos/project.yml    MARKETING_VERSION
#   README.md                 the Version column of the crate table
#
# It edits files and lock files only. It never commits, tags, pushes or
# publishes: those are done by hand, on the owner's word.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
check=0
lock=1
new=""
while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --no-lock) lock=0 ;;
    --root)
      [ $# -ge 2 ] || { echo "bump-version.sh: --root needs a directory" >&2; exit 2; }
      root="$(cd "$2" && pwd)"
      shift
      ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    -*) echo "bump-version.sh: unknown option $1" >&2; exit 2 ;;
    *)
      [ -z "$new" ] || { echo "bump-version.sh: one version, not \"$new\" and \"$1\"" >&2; exit 2; }
      new="$1"
      ;;
  esac
  shift
done

die() { echo "bump-version.sh: $*" >&2; exit 1; }

workspace="$root/Cargo.toml"
shim="$root/shim/senate/Cargo.toml"
app="$root/apps/macos/project.yml"
readme="$root/README.md"
for file in "$workspace" "$shim" "$app" "$readme"; do
  [ -f "$file" ] || die "$file is missing"
done

# The workspace version is the reference: the first bare `version = "…"`
# line of the workspace manifest ([workspace.package]).
current="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$workspace" | sed -n 1p)"
[ -n "$current" ] || die "no workspace version in $workspace"

# Each place: the lines that state a version there, one per line as
# `<file>:<line number>:<text>`.
places() {
  grep -n -H -E '^version = "' "$workspace" | sed -n 1p
  grep -n -H -E '^senate[a-z-]* = \{ path = "crates/' "$workspace"
  grep -n -H -E '^version = "' "$shim" | sed -n 1p
  grep -n -H -E '^senate-bridge = \{ path = ' "$shim"
  grep -n -H -E '^ *MARKETING_VERSION: ' "$app"
  grep -n -H -E '^\| \[`senate[a-z-]*`\]\(crates/[a-z-]*\) \| `' "$readme"
}

# The places that do not state version $1.
differing() {
  places | grep -v -F -e "\"$1\"" -e "\`$1\`" || true
}

if [ "$check" = 1 ]; then
  [ -z "$new" ] || die "--check takes no version"
  wrong="$(differing "$current")"
  if [ -n "$wrong" ]; then
    echo "bump-version.sh: the workspace is at $current, but these lines say otherwise:" >&2
    echo "$wrong" >&2
    exit 1
  fi
  echo "$current ($(places | wc -l | tr -d ' ') places agree)"
  exit 0
fi

[ -n "$new" ] || { sed -n '2,21p' "$0" >&2; exit 2; }
echo "$new" | grep -q -E '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || die "\"$new\" is not a version like 0.14.0"
[ "$new" != "$current" ] || die "the version is already $current"
wrong="$(differing "$current")"
[ -z "$wrong" ] || die "the places disagree before the change; fix them first (--check lists them)"

# Rewrite only the lines listed as places, and only the version on them.
old_re="$(echo "$current" | sed 's/\./\\./g')"
rewrite() {
  file="$1"
  shift
  tmp="$file.bump.$$"
  sed "$@" "$file" > "$tmp"
  cat "$tmp" > "$file"
  rm -f "$tmp"
}
rewrite "$workspace" \
  -e "s/^version = \"$old_re\"\$/version = \"$new\"/" \
  -e "/^senate[a-z-]* = { path = \"crates\\//s/version = \"$old_re\"/version = \"$new\"/"
rewrite "$shim" \
  -e "s/^version = \"$old_re\"\$/version = \"$new\"/" \
  -e "/^senate-bridge = { path = /s/version = \"$old_re\"/version = \"$new\"/"
rewrite "$app" \
  -e "s/^\\( *MARKETING_VERSION: \\)\"$old_re\"/\\1\"$new\"/"
rewrite "$readme" \
  -e "/^| \\[\`senate[a-z-]*\`\\](crates\\//s/| \`$old_re\` |/| \`$new\` |/"

wrong="$(differing "$new")"
[ -z "$wrong" ] || die "these lines were not changed to $new:
$wrong"
echo "bump-version.sh: $current -> $new in $(places | wc -l | tr -d ' ') places"

if [ "$lock" = 1 ]; then
  (cd "$root" && cargo update -w)
  (cd "$root/shim/senate" && cargo update -p senate-bridge -p senate-core)
  echo "bump-version.sh: both lock files refreshed"
fi
echo "bump-version.sh: nothing is committed. Next: docs/release.md, from \"Commit\"."
