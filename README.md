# OverlayBD for macOS

Native layered block storage in Rust, with a SwiftUI disk manager for macOS.
Built on a pinned [AgentENV](https://github.com/kvcache-ai/AgentENV) revision,
OverlayBD supports APFS mounts, snapshots, independent branches, layer compaction,
LZ4/Zstd archives, and on-demand reads from HTTP(S) content-addressed repositories.

**Requires Apple Silicon and macOS 26 or later.** This is an internal preview;
use data you can recover or recreate.

## Install and get started

1. Drag OverlayBD from the DMG to Applications, then open it from there.
2. Follow the setup guide to enable OverlayBDFSKit in System Settings:
   General → Login Items & Extensions → By Category → File System Extensions.
3. Once setup is complete, choose **Create Image** or **Import**. No initial image
   is included.

Creating an image verifies ownership of the new virtual disk, formats it as APFS,
and mounts it in Finder. Local imports use an export folder containing
`image.json` and its referenced layer files; importing does not format existing
image data.

The interface supports English and Simplified Chinese. Select **Follow System**,
**English**, or **Simplified Chinese** in Settings & Diagnostics → Language. The default is
Follow System, with English as the fallback for unsupported languages. Changes
apply immediately; image and snapshot names stay unchanged.

## Images, snapshots, and branches

- Set image capacity with the linked slider and input field, from 1 to 1024 GiB.
  Before creation, free space must cover the selected capacity, plus at least
  20 GiB on a system volume. Images allocate space as needed; the check does not
  reserve or preallocate storage.
- Use **Quick Branch & Mount** from an image or snapshot. For an image, the app
  first saves a starting snapshot, using an online snapshot if the image is
  mounted. For an existing snapshot, it forks directly. The new branch is named
  and mounted automatically, with independent changes.
- Online snapshots capture submitted block writes. They do not flush application
  caches or guarantee that unsaved changes are included.
- Finder volume names use the image's display name. Managed mount folders also
  include a unique ID, so images with the same name can be mounted together. The
  detail view shows the actual mount path and provides **Copy Path**. Existing
  mounts adopt updated naming on their next mount.
- The relationship graph shows current shared-layer dependencies between images,
  snapshots, and branches. It supports zoom, focus, and back navigation, and
  deduplicates shared layers in space totals.
- Delete actions are available in image details, snapshot lists, and the graph
  inspector, with confirmation. Unmount an image before deleting it. Existing
  snapshots and branches are retained. Deleting a snapshot removes that restore
  point while keeping layers referenced elsewhere; space is not reclaimed
  immediately.

Previously mounted images are restored when the app opens by default for new
users. During first-time setup, **Automatically restore disks after login** is
selected by default. Login startup is registered only when you confirm setup;
uncheck the option to leave it disabled. Existing users keep their saved settings,
and manually unmounted images are not restored.

Settings also provide hiding mounted volumes in Finder, which is off by default.
Visibility changes apply on the next mount; hidden volumes remain accessible
through their mount folders.

Quitting safely unmounts volumes first. If a volume is busy or its state is
uncertain, the app, background service, and data are retained without a forced
unmount.

## Store location and data

The default store is `~/Library/Application Support/OverlayBD/store`. App metadata
and preferences remain in the parent directory. Deleting the app does not delete
image data.

Change the entire store location from the creation dialog or Settings. Unmount all
images, then choose a parent folder. The app copies all images, snapshots, and
shared layers into a new `OverlayBD Store` subfolder and switches to it. The
original copy remains as a backup; remove it only after checking the new store.
The destination needs room for the complete copy, with an additional 20 GiB kept
free on a system volume. If an external store is disconnected, the app requests
the disk rather than creating an empty replacement store.

Public registry and resource formats start at version 1; unsupported formats are
rejected. Export immutable snapshots with the original development version and
import them into a new store. Do not change database version numbers manually.

## Build from source

Development requires Xcode, Rust/Cargo, and a C compiler toolchain. End users only
need the packaged app.

```sh
git submodule update --init --recursive
cargo build --locked --workspace
OBD_BUILD_CONFIGURATION=Release OBD_DEVELOPMENT_TEAM=YOUR_TEAM_ID ./scripts/build-macos.sh
```

Development signing and distribution signing have different requirements. See
[Development and distribution](docs/development.md) for builds, validation, and DMG
notarization.

## Command line

The app bundles the CLI and daemon in `Contents/Helpers`. Enable the command-line
tools in Settings & Diagnostics → Command Line to create `~/.local/bin/obd`.
Add `~/.local/bin` to your `PATH`, then run `obd image list` while the app is running.
The launcher uses the GUI service by default and supports `OBD_SOCKET` and
`--socket` overrides. The app refreshes an installed launcher at startup and
removes it when disabled, without overwriting unrelated files.

For development, the source-built tools can use an independent store:

```sh
# Terminal 1
./target/debug/overlaybd-macosd /tmp/obd-store

# Terminal 2
export OBD_SOCKET=/tmp/obd-store/control.sock
./target/debug/obd image create disk-a --size 1G
./target/debug/obd snapshot create disk-a --name checkpoint-a
./target/debug/obd image fork checkpoint-a disk-b
./target/debug/obd image list
./target/debug/obd --help
```

CLI creation produces an unformatted block image. The app's creation flow also
initializes APFS.

| Operation | Command |
|---|---|
| Images and snapshots | `obd image --help`, `obd snapshot --help` |
| Mount and unmount | `obd device --help` |
| Import and export | `obd image import`, `obd snapshot export` |
| Compaction and space usage | `obd snapshot compact`, `obd image usage` |
| Remote publishing and reads | `obd snapshot publish`, `obd image pull`, `obd image fetch` |
| Interrupted state and recovery | `obd leases`, `obd pending-restacks`, `obd recover --help` |

Recover an interrupted lease only after confirming that every consumer is offline.
Recovery does not repair the inner filesystem or torn writes. Shared layers survive
source-image deletion, and usage totals do not indicate how much space deletion
will release.

See [Storage and recovery](docs/implementation.md) for consistency boundaries and
[HTTP CAS](docs/http-cas.md) for the remote repository protocol. Resizing, TRIM,
private repository authentication, and the OCI Registry protocol are not exposed.

## Tests and licensing

[Testing](tests/README.md) separates regressions that need no mounts from optional
real-mount validation. The public repository includes reproducible tests and
synthetic fixtures, not runtime logs, performance samples, or personal-machine
results.

OverlayBD is licensed under [MIT](LICENSE). See
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) and the
[dependency inventory](docs/dependency-licenses.json) for pinned upstream sources,
copyrights, and dependency licenses.
