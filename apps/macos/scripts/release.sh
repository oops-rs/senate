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
#   SENATE_BUNDLED_SCENES  folder of built-in .senatescene archives to ship in
#                          the app (default apps/macos/BundledScenes; see its README)
#
# Output in build/release/: Senate.app, Senate-<version>.dmg, and the
# separate remote administrator helper and its SHA-256 checksum.
set -euo pipefail

app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scenes_src="${SENATE_BUNDLED_SCENES:-$app_dir/BundledScenes}"
[[ "$scenes_src" == /* ]] || scenes_src="$PWD/$scenes_src"
cd "$app_dir"
identity="${SENATE_SIGN_IDENTITY:-Developer ID Application}"
profile="${SENATE_NOTARY_PROFILE:-senate-notary}"
notarize=1
for arg in "$@"; do
  case "$arg" in
    --no-notarize) notarize=0 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
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
team=""
if [[ "$identity" != "-" ]]; then
  # Resolve the name to one certificate's hash: Xcode maps a bare
  # "Apple Development" to the legacy "Mac Development" certificate, which
  # a current account doesn't have.
  match="$(security find-identity -v -p codesigning | grep -F "$identity" | head -1)"
  [[ -n "$match" ]] || die "no codesigning identity matches \"$identity\""
  identity="$(awk '{print $2}' <<<"$match")"
  # The team is the certificate's organizational unit. The parenthesis in an
  # "Apple Development: Name (XXXXXXXXXX)" name is the person's own id, not
  # the team, so read the certificate that has this hash.
  team="$(security find-certificate -a -Z -p \
    | awk -v want="$identity" '
        /^SHA-1 hash:/ { keep = ($3 == want) }
        keep && /BEGIN CERTIFICATE/ { pem = 1 }
        keep && pem { print }
        keep && /END CERTIFICATE/ { exit }' \
    | openssl x509 -noout -subject -nameopt multiline 2>/dev/null \
    | sed -n 's/^ *organizationalUnitName *= *//p' | head -1)"
  [[ -n "$team" ]] || die "could not read the team of the signing certificate $identity"
  team_flags=(DEVELOPMENT_TEAM="$team")
fi
say "building the separate remote administrator artifact and CLI"
repo="$app_dir/../.."
helper_timestamp=--timestamp=none
[[ $notarize == 1 ]] && helper_timestamp=--timestamp
SENATE_REMOTE_ADMIN_SIGN_IDENTITY="$identity" SENATE_REMOTE_ADMIN_TIMESTAMP="$helper_timestamp" \
  "$repo/deploy/build-remote-admin.sh" "$app_dir/$out"
helper_sha="$(awk 'NR == 1 { print $1 }' "$out/senate-remote-admin.sha256")"
[[ "$helper_sha" =~ ^[0-9a-f]{64}$ ]] || die "invalid administrator artifact checksum"
export SENATE_REMOTE_ADMIN_SHA256="$helper_sha"
if [[ -n "$team" ]]; then
  export SENATE_REMOTE_ADMIN_TEAM_ID="$team"
  export SENATE_PERSON_TEAM_ID="$team"
else
  unset SENATE_REMOTE_ADMIN_TEAM_ID
  unset SENATE_PERSON_TEAM_ID
fi
cargo build --manifest-path "$repo/Cargo.toml" --locked --release --target-dir "$repo/target" \
  -p senate-bridge -p senated --bin senate --bin senated
cp "$repo/target/release/senate" "$out/senate"
cp "$repo/target/release/senated" "$out/senated"
if [[ -n "$team" ]]; then
  codesign --force --options runtime --identifier dev.senate.daemon \
    --sign "$identity" "$helper_timestamp" "$out/senated"
  codesign --verify --strict \
    -R "=anchor apple generic and identifier \"dev.senate.daemon\" and certificate leaf[subject.OU] = \"$team\"" \
    "$out/senated"
fi

# Built-in scenes: each archive checked by this checkout's `senate scene
# inspect` (the app's own rules), staged, and copied into the app's
# Resources/Scenes by the build's "Copy built-in scenes" phase, before signing.
scenes_stage="$app_dir/build/bundled-scenes"
rm -rf "$scenes_stage"
mkdir -p "$scenes_stage"
shopt -s nullglob
for scene in "$scenes_src"/*.senatescene; do
  report="$("$out/senate" scene inspect "$scene")" || die "built-in scene $scene is refused (above)"
  fingerprint="$(awk '$1 == "fingerprint" { print $2 }' <<<"$report")"
  say "built-in scene $(basename "$scene"): $(head -1 <<<"$report"), fingerprint $fingerprint"
  cp "$scene" "$scenes_stage/"
done
shopt -u nullglob
scene_count="$(find "$scenes_stage" -name '*.senatescene' | wc -l | tr -d ' ')"
say "$scene_count built-in scene(s) from $scenes_src"

xcodebuild -project Senate.xcodeproj -scheme Senate -configuration Release \
  -derivedDataPath build -archivePath "$archive" archive \
  CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$identity" ${team_flags[@]+"${team_flags[@]}"} \
  SENATE_BUNDLED_SCENES_STAGE="$scenes_stage" \
  OTHER_CODE_SIGN_FLAGS="$helper_timestamp" | grep -E "^(\*\*|error|warning: [^S])" || true
[[ -d "$archive/Products/Applications/Senate.app" ]] || die "the archive has no Senate.app"
ditto "$archive/Products/Applications/Senate.app" "$out/Senate.app"
app="$out/Senate.app"
shipped="$(find "$app/Contents/Resources/Scenes" -name '*.senatescene' 2>/dev/null | wc -l | tr -d ' ')"
[[ "$shipped" == "$scene_count" ]] || die "the app carries $shipped built-in scene(s), expected $scene_count"

say "checking the signature"
codesign --verify --deep --strict --verbose=2 "$app"
details="$(codesign -dv --entitlements - "$app" 2>&1)"
grep -q "flags=.*runtime" <<<"$details" || die "the hardened runtime is not on"
grep -q "com.apple.security.automation.apple-events" <<<"$details" || die "the Apple Events entitlement is missing"
if [[ "$identity" != "-" && $notarize == 1 ]]; then
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
# A secure timestamp needs Apple's timestamp service, and only a notarized
# build needs one: without notarizing, sign without it.
timestamp=(--timestamp)
[[ "$identity" == "-" || $notarize == 0 ]] && timestamp=()
codesign --force --sign "$identity" ${timestamp[@]+"${timestamp[@]}"} "$dmg"
if [[ $notarize == 1 ]]; then
  say "notarizing the DMG"
  xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait
  xcrun stapler staple "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi

say "done: $app, $dmg"
if [[ $notarize == 0 ]]; then
  echo "release.sh: not notarized (--no-notarize): Gatekeeper will refuse it on another Mac."
fi
