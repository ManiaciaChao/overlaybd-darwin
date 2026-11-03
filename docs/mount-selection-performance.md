# Mount selection performance

Measured locally on macOS on 2026-11-02 using disposable APFS sparse images.
Volume labels remain distinct; the host's rename step is unchanged.

## Change

For ordinary GPT/APFS layouts, mount selection uses the disk and APFS listings
to identify candidates, then queries only the selected volume. It no longer
queries the whole disk and APFS physical-store partition as filesystem candidates.
An explicit volume device path skips both listings. Both paths still observe
the current FSKit/RAW attachment and verify that the selected volume's physical
storage belongs to it. Post-mount identity and mountpoint checks remain intact.

Unknown layouts use the existing full discovery path. Automatic selection still
rejects multiple data volumes, and EFI remains explicitly selectable through the
fallback. No candidate information is cached across requests.

## Measurements

Five comparisons on an attached, unmounted, single-volume 128 MiB APFS image:

| Run | Previous selection (ms) | New automatic selection (ms) | Explicit device (ms) |
| --- | ---: | ---: | ---: |
| 1 | 837.5 | 501.5 | 112.3 |
| 2 | 530.0 | 427.4 | 124.3 |
| 3 | 532.0 | 436.2 | 128.5 |
| 4 | 894.5 | 468.8 | 128.2 |
| 5 | 624.4 | 455.5 | 109.6 |
| Mean | 683.7 | 457.9 | 120.6 |

Automatic selection saved approximately 226 ms (33%) in this sample. The GUI
uses automatic selection, so the explicit-device result is not its current
end-to-end performance. These measurements exclude the unchanged attachment
observation, actual mount, rename, and post-mount validation. Old and new results
were compared for equality on every iteration.

Separately, five `diskutil mount -mountPoint` / `renameVolume` / unmount cycles
on the same disposable image measured:

| Run | Mount (ms) | Rename (ms) |
| --- | ---: | ---: |
| 1 | 12603.7 | 153.1 |
| 2 | 386.2 | 154.4 |
| 3 | 329.7 | 161.0 |
| 4 | 199.9 | 156.1 |
| 5 | 308.2 | 160.9 |

Rename averaged 157.1 ms. Mount averaged 306.0 ms after the first attempt; the
first mount took 12.6 seconds. Follow-up unified-log inspection attributed most
of this delay to StorageKit's post-mount metadata refresh (see below).
This is a local DiskImages test, not a complete OverlayBD live-fork
benchmark, and should not be used to predict FSKit or engine latency.

Validation also used a separate 1 GiB image with two APFS volumes: automatic
selection rejected ambiguity, UUID and explicit-device selection matched full
discovery, and a volume on another disk was rejected. The daemon's seven unit
tests and five recovery integration tests passed.

## First-mount delay attribution

The original run's macOS unified logs remained available. Inspecting
`diskarbitrationd`, `storagekitd`, and `diskutil` gives this timeline
(2026-11-02, Asia/Singapore):

| Time | Process | Event |
| --- | --- | --- |
| 16:21:10.634 | diskarbitrationd | Mount request queued for the disposable APFS volume |
| 16:21:10.635–10.694 | diskarbitrationd | APFS probe/check completes, fsck status 0 |
| 16:21:10.695–10.698 | diskarbitrationd | Mount approval callback approved |
| 16:21:10.698–10.792 | diskarbitrationd | Actual mount completes successfully |
| 16:21:10.794 | diskarbitrationd | Mount-success callback dispatched |
| 16:21:10.919–20.374 | storagekitd | Caches volume metadata; CacheDelete/purgeable-space work completes |
| 16:21:20.376–23.206 | storagekitd | Performs another full recache, including CacheDelete/purgeable-space work |
| 16:21:23.208 | storagekitd | `Finished operation: SKMountOperation` |
| 16:21:23.208 | diskutil | Receives XPC reply for `mountDisk:options:blocking:completionBlock:` |

Thus Disk Arbitration's mount stage completed in about 160 ms. Approximately
12.4 seconds elapsed **after mount success but before diskutil received its
StorageKit completion reply**. The two intervening volume-refresh intervals
contain `com.apple.cache_delete` connections and
`CacheDeleteCopyPurgeableSpaceWithInfo` results. This establishes the delayed
stage as StorageKit's post-mount refresh; the deepest internal reason for the
CacheDelete latency is not established without further tracing.

The observation does not support attributing the 12.6 seconds to APFS mounting,
fsck, volume rename, or OverlayBD block I/O.

## Direct Disk Arbitration mounting

The daemon now calls `DADiskMountWithArguments` directly instead of spawning
`diskutil mount`. It waits for the mount-completion callback on a blocking
worker's private CFRunLoop mode, with a 90-second deadline. Session scheduling
and CoreFoundation references are released on both success and failure. A
failure is returned without issuing a second mount through another mechanism.

The existing pre-mount RAW/volume ownership checks and post-mount RAW, physical
storage, UUID, filesystem and mountpoint checks remain. Read-only and nobrowse
are forwarded as `rdonly` and `nobrowse`. The host still assigns the branch's
distinct APFS label with `renameVolume` after mounting.

API contract: [Apple's DADiskMountWithArguments documentation](https://developer.apple.com/documentation/diskarbitration/dadiskmountwitharguments(_:_:_:_:_:_:)).

An opt-in test creates a fresh 1 GiB sparse image for each arm in two rounds,
then mounts, validates, renames and unmounts it three times. Timings include
separate measurements for existing post-mount system queries, to catch any
StorageKit delay simply moving from mount to validation.

| Fresh-image first mount | Mount (ms) | Validation (ms) | Rename (ms) | Sum (ms) |
| --- | ---: | ---: | ---: | ---: |
| Round 1, direct DA | 199.1 | 415.9 | 147.1 | 762.1 |
| Round 1, diskutil | 12942.5 | 240.8 | 140.4 | 13323.7 |
| Round 2, direct DA | 197.1 | 559.1 | 149.5 | 905.7 |
| Round 2, diskutil | 36429.2 | 619.4 | 180.6 | 37229.2 |

All six direct-DA mounts took 138–248 ms. Their post-mount validation took
416–734 ms, so the long delay was not simply moved to validation in this sample.
The diskutil arm also had a 25.50-second delay on a later mount: the problem is
not limited to a volume's first mount. These are local observations, not a
guarantee of latency on another host or a complete fork-operation benchmark.

The test also verifies that readonly/nobrowse flags reach the mounted filesystem
using `statfs`, that rename preserves the requested mountpoint, and that a bad
mountpoint produces an error.

```sh
cargo test -p obd-daemon --bin overlaybd-macosd \
  benchmark_disk_arbitration_mount -- --ignored --nocapture
```

This test performs real mounts only on the images it creates. It retains image
files on failure and removes them only after successful detachment.

### Initial host integration limitation (resolved by matching the extension)

The current-source host integration test was attempted with the locally built
daemon and enabled installed FSKit extension. It failed during `device create`,
at FSKit resource loading with `EIO`, before the new APFS mount code was invoked.
Its resource descriptor uses schema version 1. A separate isolated baseline
using the installed app's unmodified daemon successfully created/detached an
FSKit device and emitted a schema-version-3 resource descriptor. The installed
extension and this checkout's resource schema therefore differ; the failure
does not exercise the changed APFS mount operation.

The failed test's devices were removed through the controller after confirming
both presentation layers offline. Leases were empty and its daemon was stopped.
The detached store is retained at
`/private/tmp/obd-gui-06230493-4A44-4BE1-93CE-B8BA37DA284E` for inspection.
The baseline daemon was also stopped after detaching its test device. The
installed app and extension were not replaced in that initial attempt. Full
live-fork GUI integration was unverified at that point; the later validation
below resolves that gap.

## Validation after the UI and packed-upper integration

On 2026-11-02, validated commit `5a51e87` plus the working-tree startup-navigation
adjustment. Rust workspace tests and the raw-volume callback tests passed. Host
tests passed for startup/navigation, graph logic, compression policies, live
storage sampling, actual CLI/store integration, and store relocation.

The first host run used stale default Release tools; rerunning with current
Debug tools resolved its missing-policy assertion. Relocation initially hit the
20 GiB reserve on the system disk (8.4 GiB available). `HostStorageTests` now
accepts `OBD_TEST_ROOT`; relocation passed on the workspace volume without
changing the production reserve or socket-path limits.

Built a signed matching app/extension with the existing development identity.
Registering it at the development-app path did not change the extension actually
launched by FSKit, so the installed app was backed up and updated in place after
checking that its devices/leases and all obdraw mounts were empty. Original
bundle directories were retained. The original app backup is at
`macos/build/verify-before-native-73c502b4-a296-4f23-94de-a1c716ee9ffa.app`.
The updated app opens to Overview and reports `Service ready`.

The full host integration test then passed against the installed helpers:
APFS initialization beside EFI, Unicode/space-containing names and mountpoints,
distinct source/fork APFS labels, live snapshot, simultaneous fork read/write
isolation, detach/reopen, export/import, same-label mount isolation, usage, and
safe shutdown. It retained its detached test store at
`/private/tmp/obd-gui-AE2511C3-57E5-4E84-B6D2-C374B6CE6F4F`.

Initial attach/mount/verify/rename totals were 2.409 s for the source and 6.813 s
for the fork. The DA request-to-mount-success intervals in the same run were
104 ms and 263 ms respectively. Those totals therefore must not be described
as pure APFS mount times or compared directly with the earlier sparse-image
microbenchmark.

### Remaining latency: pre-mount disk enumeration

A timed reopen of that detached test store passed as well:

| Operation | Source | Fork |
| --- | ---: | ---: |
| Device create/attach | 1.070 s | 1.066 s |
| Device mount, including discovery and validation | 3.182 s | 13.620 s |
| Total (labels already matched; no rename necessary) | 4.252 s | 14.687 s |

For the fork, the daemon launched `diskutil list -plist /dev/disk8` and the
global `diskutil apfs list -plist` concurrently at 17:51:29.558. Execution audit
timestamps show plist conversion starting at 17:51:39.627 and 17:51:41.929,
after those queries returned. Thus discovery waited approximately 12.37 s.
The selected-volume info query then started at 17:51:41.931; DA received the
mount request at 17:51:42.182 and completed it at 17:51:42.322 (140 ms).

Direct DA mounting is functionally verified and avoids waiting for StorageKit's
mount-operation completion, but **end-to-end latency is not fully solved**.
The remaining `diskutil list`/global APFS discovery can still stall before DA
mount is even requested. The source's discovery pair took about 1.4 s in the
same reopen run. Eliminating those queries is a separate remaining optimization;
the per-candidate-query reduction alone does not eliminate global enumeration.

Both verification runs detached all their devices and stopped their test
daemons. The existing user library was not used as a test fixture.

## Native volume discovery

Replaced both `diskutil list` and `diskutil apfs list` with IOKit traversal
rooted at the currently observed RAW disk's IOMedia, plus
`DADiskCopyDescription` for filesystem, volume UUID/name and mountpoint. Status,
mount selection and unmount all use this discovery path. There is no command-
line enumeration fallback. Targeted `diskutil info` ownership/identity checks
and the host's `renameVolume` call remain; this is not a claim that every
control-plane command has been replaced by an API.

All IOMedia ancestors of a discovered volume must remain under the exact RAW
root. Containers spanning foreign storage are rejected. The root registry ID
is rechecked after traversal; invalidated iterators fail rather than publish
an incomplete enumeration. Mounted-volume metadata is checked against statfs.
See [Apple's IOKit traversal API](https://developer.apple.com/documentation/iokit/1514318-ioregistryentrycreateiterator)
and [Disk Arbitration's IOMedia interface](https://developer.apple.com/documentation/diskarbitration/dadiskcopyiomedia(_:)).

An opt-in real-image test measured native discovery at **4.321 ms**. It also
checks two separate same-label disks, foreign-device rejection, discovery of a
newly added APFS volume, ambiguous automatic selection, and UUID selection:

```sh
cargo test -p obd-daemon --bin overlaybd-macosd \
  native_volume_discovery_checks_topology_and_ambiguity -- --ignored --nocapture
```

Reopening the same retained source/fork fixture used above:

| Operation | Source | Fork |
| --- | ---: | ---: |
| Device create/attach | 1.246 s | 0.584 s |
| Device mount, including native discovery and validation | 0.695 s | 0.718 s |
| Total (unchanged labels) | **1.942 s** | **1.302 s** |

The previous fork total was 14.687 s. This comparison uses the same store and
operation boundaries, and includes attachment/ownership work; it is not merely
the DA mount callback duration. Timings remain host-dependent observations.

A fresh complete host integration run also passed. Fresh source and fork
attach/mount/verify/rename totals were 3.124 s and 2.722 s respectively. The
fork's components were 1.015 s device creation, 1.522 s mount/discovery/validation,
and 185 ms rename. It verified real EFI exclusion, unique source/fork labels,
live snapshot inheritance and write isolation, detach/reopen, export/import,
same-label mount isolation and safe shutdown. All devices were detached and
the daemon stopped; its retained store is
`/private/tmp/obd-gui-C01AAEBC-BA26-469C-B301-5FC782C080B8`.

The daemon's six unit tests and seven recovery integration tests passed, as did
the opt-in native discovery test. The new implementation preserves UUID/device
selection and ambiguity handling while removing the global enumeration stall
observed in the earlier run.

The signed native-discovery app was installed at `/Applications/OverlayBD.app`
after confirming no user devices, RAW leases, or obdraw mounts were active.
The previous bundle is retained at
`macos/build/before-native-discovery-351a206c-d3b0-4197-a15d-ccfa4e1f60a0.app`.
The installed daemon's SHA-256 matched the signed build. Repeating the retained-
store reopen with the **installed helpers** measured:

| Operation | Source | Fork |
| --- | ---: | ---: |
| Device create/attach | 1.860 s | 0.748 s |
| Device mount/discovery/validation | 0.749 s | 0.808 s |
| Total (unchanged labels) | **2.610 s** | **1.556 s** |

Both volumes were detached and the test daemon stopped successfully.

## Reproduction

Attach a disposable APFS image with `hdiutil attach -nomount`, then run:

```sh
OBD_BENCH_DEVICE=/dev/diskN cargo test -p obd-daemon --bin overlaybd-macosd \
  benchmark_mount_selection -- --ignored --nocapture
```

The ignored test only reads disk metadata. For a multi-volume image, set
`OBD_BENCH_VOLUME` to the desired volume UUID; the test also checks that automatic
selection rejects ambiguity. Set `OBD_BENCH_FOREIGN_VOLUME` to a volume device
on another disk to check that explicit selection rejects foreign storage.
Detach the disposable image after testing.
