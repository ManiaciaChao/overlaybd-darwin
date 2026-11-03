# Development, validation, and distribution

## Build

The `vendor/agentenv` submodule pins the AgentENV fork revision. Cargo.lock
pins Rust dependencies. Development requires Apple Silicon, macOS 26 or later,
Xcode, and Rust/Cargo.

```sh
git submodule update --init --recursive
cargo build --locked --workspace
OBD_DEVELOPMENT_TEAM=YOUR_TEAM_ID OBD_BUILD_CONFIGURATION=Release ./scripts/build-macos.sh
```

Without a development team, the script produces an ad-hoc build artifact. This
does not establish that the FSKit extension can be installed or distributed.
Device registration requires an explicit opt-in from the account owner; the
script does not register development devices by default.

Before updating a local development installation, safely unmount all obdraw
volumes, then run:

```sh
python3 scripts/install-development.py macos/build/xcode/Build/Products/Release/OverlayBDHost.app
```

The extension is packaged in `Contents/Extensions`. FSClient checks whether it is
enabled in System Settings; pluginkit registration or a setup-button click does
not establish permission. The Developer ID host app runs without App Sandbox and
manages the bundled tools. The FSKit extension remains sandboxed and receives
scoped resource access.

## Validate

```sh
python3 scripts/audit-public.py
cargo fmt --all -- --check
cargo build --locked --workspace
cargo test --locked --workspace
./scripts/test-macos.sh
./scripts/test-host.sh
```

Build the workspace first so the independent worker uses the same format as the
tests. See [Testing](../tests/README.md) for coverage, synthetic fixtures, and
optional real-mount validation. Generated results belong in ignored directories;
do not commit raw logs or runtime data.

## GitHub Actions

See [GitHub Actions setup](github-actions.md) for CI coverage, release environment
secrets, and tag-triggered signed and notarized release drafts.

## Sign and package a DMG

Distribution requires a Developer ID Application certificate, an FSKit Developer
ID provisioning profile that permits all devices, and a configured notarytool
keychain profile. Keep private keys, passwords, provisioning profiles, and
notarization logs out of the repository.

```sh
OBD_DEVELOPMENT_TEAM=YOUR_TEAM_ID OBD_NOTARY_PROFILE=YOUR_PROFILE ./scripts/package-dmg.sh
```

The pipeline creates a Release archive and Developer ID export, checks nested
signatures, profiles, and embedded build paths, then creates, notarizes, and
staples the DMG and checks it with Gatekeeper. The app includes the CLI, daemon,
worker, and license materials. The DMG includes English installation instructions
as `Installation.txt`.

The output is `macos/build/OverlayBD-0.1.0-arm64.dmg`. The entire build directory
is excluded from version control.

Set `OBD_SKIP_ARCHIVE=1` to reuse an existing archive. An intermediate package
with an `-unnotarized` suffix is allowed only when
`OBD_SKIP_NOTARIZATION=1` is explicitly set; it is not a notarized release.

Release builds remap Rust, Clang, and Swift debug paths and use concise Swift file
identifiers. Distribution checks also scan the final executables for the current
workspace and user-directory paths. The Developer ID holder's name and Team ID
are part of the valid public signature and cannot be removed while retaining it.

## Localization

The host supports English and Simplified Chinese. Its language setting defaults
to Follow System, with English as the fallback. Language preferences are saved
separately from image metadata and apply immediately.

Keep interface text in `macos/OverlayBDHost/en.lproj/Localizable.strings` and
`macos/OverlayBDHost/zh-Hans.lproj/Localizable.strings`. English keys provide the
fallback text. Use `L10n.tr` for app-owned strings and preserve matching `%@`
placeholders in both catalogs. The Xcode project and shell build both package
the resources.

`python3 scripts/check-localizations.py` checks coverage and placeholders;
`./scripts/test-host.sh` also tests language resolution, persistence, and runtime
catalog loading without modifying the user's language preferences.

## Source distribution and licensing

The project as a whole uses `GPL-3.0-or-later`. Preserve LICENSE and
THIRD_PARTY_NOTICES when distributing it; bundled third-party components retain
their original licenses. The packaging script copies both notices and the
third-party license inventory into `Contents/Resources/Licenses`.

When publishing a binary release, provide equivalent access to its corresponding
source and clear source-download instructions next to the binary download. Include
the exact pinned `vendor/agentenv` source and the build scripts needed for that
release. GitHub's automatically generated parent-repository source archives do
not include submodule contents and are not sufficient on their own. Retain access
to the source for as long as required by the license. See LICENSE, section 6.

## Data and updates

The first public registry schema and resource manifest versions are both 1. The
control protocol is independently versioned and is also 1. Unsupported formats
are rejected; development data is not automatically relabeled or migrated in
place.

Quit normally and unmount every volume before updating. Export snapshots with the
original binary when converting older data, then import them into a fresh store.
The app's `library.json` holds display names, origins, mount visibility, and mount
restoration preferences. The Rust registry is the source of truth for storage
state. Moving a store preserves the original copy and keeps app metadata in the
application-support directory.

## Packed upper integration

The optional packed upper, compression policy, live storage statistics, and FSKit
I/O fences are integrated here. The default image backend remains unchanged.
See [packed-upper.md](packed-upper.md) for format, durability, compatibility, and
historical performance evidence. Historical results are not a new performance
acceptance run for this merged tree.

```sh
cargo build --locked --workspace  # refresh the CLI, daemon and offline worker
cargo test --locked --workspace
OBD_TEST_STORE=1 OBD_TEST_TOOLS="$PWD/target/debug" ./scripts/test-host.sh
./scripts/test-macos.sh
```

The historical VDO comparison is independent of packed and requires an external
Darwin VDO worker. Build it with `OBD_VDO_SOURCE=/path/to/vdo-darwin ./scripts/build-vdo.sh`, then run
`cargo test -p obd-core-adapter --features experimental-vdo --test vdo`.
It is not required by the default build or tests.

The fork commit must be pushed and verified accessible before publishing the
parent repository; see [upstream-revisions.md](upstream-revisions.md).

Integration validation (2026-11-02): workspace build and tests passed (60 passed,
6 ignored); native engine tests passed (445 passed, 2 ignored); host suites with
the real isolated CLI/daemon store, Swift typechecking, FSKit callback tests and
workspace Clippy passed. Existing warnings remain. The compression host test
waits separately for operation completion and asynchronous usage publication.
No new mounted FSKit performance run or installed-app update is claimed here.
The public-source audit allows the reviewed README logo by its exact SHA-256
hash; other unexpected binary artifacts remain rejected.
