#!/usr/bin/env bash
# Builds Senate.app for distribution outside the App Store: a Release
# build signed with a Developer ID, hardened runtime and the app's
# entitlements, notarized and stapled, in a signed, notarized, stapled DMG.
#
#   scripts/release.sh                  sign, notarize, staple, DMG
#   scripts/release.sh --no-notarize    stop after signing (and the DMG)
#
# Environment:
#   SENATE_SIGN_IDENTITY   codesign identity (default "Developer ID Application");
#                          "-" signs ad hoc, to check the pipeline without one
#   SENATE_NOTARY_PROFILE  notarytool keychain profile (default "senate-notary"),
#                          made once with `xcrun notarytool store-credentials`
#
# Output in build/release/: Senate.app, Senate-<version>.dmg, and the
# separate remote administrator helper and its SHA-256 checksum.
set -euo pipefail

app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$app_dir"
identity="${SENATE_SIGN_IDENTITY:-Developer ID Application}"
profile="${SENATE_NOTARY_PROFILE:-senate-notary}"
notarize=1
for arg in "$@"; do
  case "$arg" in
    --no-notarize) notarize=0 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "release.sh: unknown argument $arg" >&2; exit 2 ;;
  esac
done

say() { printf '\nrelease.sh: %s\n' "$*"; }
die() { printf '\nrelease.sh: %s\n' "$*" >&2; exit 1; }

version="$(sed -n 's/^ *MARKETING_VERSION: *"\(.*\)"/\1/p' project.yml)"
out="build/release"
archive="build/Senate.xcarchive"
rm -rf "$out" "$archive"
mkdir -p "$out"

if [[ "$identity" != "-" ]]; then
  security find-identity -v -p codesigning | grep -q "$identity" || die "no codesigning identity matching \"$identity\" in the keychain.
  A Developer ID Application certificate is needed to distribute outside the App Store:
    1. In the Apple Developer account (Certificates), create a \"Developer ID Application\" certificate,
       or in Xcode › Settings › Accounts › Manage Certificates › + › Developer ID Application.
    2. It lands in the login keychain; check with: security find-identity -v -p codesigning
  To check this pipeline without one: SENATE_SIGN_IDENTITY=- scripts/release.sh --no-notarize"
fi
if [[ $notarize == 1 && "$identity" == "-" ]]; then
  die "an ad hoc signature cannot be notarized; use --no-notarize, or a Developer ID identity"
fi
if [[ $notarize == 1 ]] && ! xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
  die "no notarytool profile \"$profile\". Make it once (an app-specific password from appleid.apple.com):
    xcrun notarytool store-credentials $profile --apple-id <apple id> --team-id <team id> --password <app-specific password>
  or stop before notarizing: scripts/release.sh --no-notarize"
fi

say "building Senate $version (Release)"
xcodegen generate --quiet
team_flags=()
if [[ "$identity" != "-" ]]; then
  # Resolve the name to one certificate's hash: Xcode maps a bare
  # "Apple Development" to the legacy "Mac Development" certificate, which
  # a current account doesn't have.
  match="$(security find-identity -v -p codesigning | grep -F "$identity" | head -1)"
  [[ -n "$match" ]] || die "no codesigning identity matches \"$identity\""
  team="$(sed -n 's/.*(\([A-Z0-9]*\))".*/\1/p' <<<"$match")"
  identity="$(awk '{print $2}' <<<"$match")"
  team_flags=(DEVELOPMENT_TEAM="$team")
fi
xcodebuild -project Senate.xcodeproj -scheme Senate -configuration Release \
  -derivedDataPath build -archivePath "$archive" archive \
  CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$identity" ${team_flags[@]+"${team_flags[@]}"} \
  OTHER_CODE_SIGN_FLAGS="--timestamp" | grep -E "^(\*\*|error|warning: [^S])" || true
[[ -d "$archive/Products/Applications/Senate.app" ]] || die "the archive has no Senate.app"
ditto "$archive/Products/Applications/Senate.app" "$out/Senate.app"
app="$out/Senate.app"

say "checking the signature"
codesign --verify --deep --strict --verbose=2 "$app"
details="$(codesign -dv --entitlements - "$app" 2>&1)"
grep -q "flags=.*runtime" <<<"$details" || die "the hardened runtime is not on"
grep -q "com.apple.security.automation.apple-events" <<<"$details" || die "the Apple Events entitlement is missing"
if [[ "$identity" != "-" ]]; then
  grep -q "Timestamp=" <<<"$details" || die "no secure timestamp (notarization needs one)"
fi
echo "$details" | grep -E "^(Identifier|Authority|TeamIdentifier|Timestamp|CodeDirectory)" || true

if [[ $notarize == 1 ]]; then
  say "notarizing the app"
  ditto -c -k --keepParent "$app" "$out/Senate.zip"
  xcrun notarytool submit "$out/Senate.zip" --keychain-profile "$profile" --wait
  xcrun stapler staple "$app"
  rm "$out/Senate.zip"
  spctl --assess --type execute --verbose=2 "$app"
fi

say "making the DMG"
dmg="$out/Senate-$version.dmg"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Senate.app"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname "Senate $version" -srcfolder "$stage" -ov -format UDZO "$dmg"
timestamp=(--timestamp)
[[ "$identity" == "-" ]] && timestamp=()
codesign --force --sign "$identity" ${timestamp[@]+"${timestamp[@]}"} "$dmg"
if [[ $notarize == 1 ]]; then
  say "notarizing the DMG"
  xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait
  xcrun stapler staple "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi

say "building the separate remote administrator artifact"
"$app_dir/../../deploy/build-remote-admin.sh" "$app_dir/$out"

say "done: $app, $dmg"
if [[ $notarize == 0 ]]; then
  echo "release.sh: not notarized (--no-notarize): Gatekeeper will refuse it on another Mac."
fi
