# Releasing Senate

A release is the Rust crates on crates.io, a `vX.Y.Z` tag, and the macOS app
built from the same commit. One script does the part that is only editing:
`deploy/bump-version.sh`. Everything that leaves the machine (push, tag,
publish) is done by hand, on the owner's word, and the script does none of it.

## 1. Before

On `main`, with everything for the release merged and the tree clean, one
clean run of the gate:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast -- --test-threads=4
```

## 2. Version

```sh
deploy/bump-version.sh 0.14.0
```

It writes the version in every place that states it and refreshes both lock
files:

| File | What |
| :--- | :--- |
| `Cargo.toml` | the workspace version, and the version of each workspace crate in `[workspace.dependencies]` |
| `shim/senate/Cargo.toml` | the package version, and `senate-bridge`'s |
| `apps/macos/project.yml` | `MARKETING_VERSION` |
| `README.md` | the Version column of the crate table |
| `Cargo.lock` | `cargo update -w` |
| `shim/senate/Cargo.lock` | `cargo update -p senate-bridge -p senate-core`, run in `shim/senate` |

`deploy/bump-version.sh --check` says the version and fails when a place
states another one; the test `release_version` runs it with the workspace
tests, so a place left behind fails the gate. `--no-lock` leaves the lock
files alone. A place that is not in the table above is not known to the
script: add it there and to the script together.

## 3. Commit

Review the diff: it is the version in those files and the workspace crates'
entries in the two lock files, nothing else. Then:

```sh
git commit -am "Release 0.14.0"
```

## 4. Dry run

```sh
cargo publish --workspace --exclude senate-e2e --exclude senate-app-core --dry-run
```

`senate-e2e` and `senate-app-core` are `publish = false`. The dry run
packages and builds every crate as crates.io will see it; fix what it finds
and amend the release commit before going on.

## 5. Tag, push, publish

Each of these is done by hand, on the owner's word:

```sh
git tag v0.14.0
git push origin main v0.14.0
cargo publish --workspace --exclude senate-e2e --exclude senate-app-core
cargo publish --manifest-path shim/senate/Cargo.toml
```

The workspace first, then the shim: `shim/senate` is the `senate` package on
crates.io, outside the workspace, and depends on the `senate-bridge` just
published. Within the workspace cargo orders the crates by their
dependencies (`senate-remote-admin` before `senate-bridge`, which depends
on it).

A published version cannot be replaced. If something is wrong after this
step, the fix is the next patch version.

## 6. The macOS app

From the release commit, in `apps/macos`:

```sh
scripts/release.sh
```

It builds, signs with the Developer ID, notarizes and staples the app and
its DMG, and builds the remote administrator helper with its checksum, into
`apps/macos/build/release/`. It reads the version from `project.yml`.

- `SENATE_SIGN_IDENTITY`: the codesign identity (default `Developer ID
  Application`).
- `SENATE_NOTARY_PROFILE`: the notarytool keychain profile (default
  `senate-notary`).
- `SENATE_BUNDLED_SCENES`: the folder of built-in scenes shipped in the app
  (default `apps/macos/BundledScenes`; its README says what goes there).

For a build that stays on this machine, sign with a local identity and skip
notarizing:

```sh
SENATE_SIGN_IDENTITY="Apple Development" scripts/release.sh --no-notarize
```

Gatekeeper refuses such a build on another Mac. `scripts/release.sh --help`
lists the same.

Attach the DMG to the release for the tag. The remote administrator helper
(`senate-remote-admin` and its `.sha256`) is experimental and is attached
only when the owner says so; when it is, put its checksum in the release
notes (the README's "Remote machines" says why).

## 7. Installing

Installing the new version on a machine that is in use is its own step, and
is not part of releasing. Before it, the person runs the staged check from
their own terminal, against the signed binaries `scripts/release.sh` left in
`apps/macos/build/release/`:

```sh
deploy/stage-check.sh apps/macos/build/release ~/.senate/bin/senate
```

It starts the signed daemon as a disposable service with its own home beside
the live one, touches nothing live, and ends with `READY TO INSTALL` or
`NOT READY` and the failing step. Install only after `READY TO INSTALL`.

On a Mac, install the signed binaries the check ran against, not ones built
by `cargo install`: an unsigned build cannot prove a person's terminal, so
person-only commands would be refused.

1. Back up `~/.senate/state.db` and the log database, and keep the current
   app and binaries where they can be put back.
2. Replace `/Applications/Senate.app` and `~/.cargo/bin/senate` with the new
   ones. Remove the old file first, then copy: macOS kills a signed binary
   that was overwritten in place.
3. `senate setup service install --yes --senated apps/macos/build/release/senated --app /Applications/Senate.app`
   replaces the running service.
4. `senate setup doctor` says whether the service runs the version
   installed; `senate session restart --all-outdated --dry-run` lists the
   hosted sessions still on the old host.

Elsewhere: `cargo install senate senated`, then
`senate setup service install --yes`.
