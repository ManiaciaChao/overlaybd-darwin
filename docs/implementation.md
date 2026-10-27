# Storage and recovery boundaries

## Components

The data path is `APFS → DiskImages → FSKit → C ABI → Rust ImageFile`. The extension
process reads and writes blocks directly. The daemon manages images, devices,
references, and persistent operations; the SwiftUI host uses the bundled CLI to
call the same control interface.

The app stores display names and startup preferences in `library.json`; the Rust
registry is authoritative for storage state. A presentation-layer failure is not
proof of device state and must not justify deleting backing data or releasing a
lease.

## Store and formats

```text
store/
  state.db                 # Schema 1; SQLite WAL / FULL / fullfsync
  layers/                  # Immutable shared lower layers
  images/<storage UUID>/   # Per-image resources, independent upper layers, leases, receipts
  devices/                 # Device presentation intents and FSKit proxies
```

The first public registry and resource manifest versions are both 1. Unsupported
formats or fields are rejected. The control protocol has its own version, also
1. Internal objects use UUIDs. Canonicalized data paths must remain inside the
authorized image resource directory. A single daemon holds the store's exclusive
file lock; ownership is not inferred from a PID file.

The management CLI uses a private Unix socket with UID checks. The extension's
control thread uses loopback TCP with separate random credentials for each image,
scoped to that image's claim/poll operations. Frames have a size limit and do not
carry block data. Credentials are stored in private descriptors and omitted from
ordinary listings and diagnostic summaries.

## Images and snapshots

Only immutable lower layers are shared. Each writable image has an independent
upper layer. Explicit zero writes mask lower-layer data and do not imply physical
space reclamation. Deletion removes persistent references first; GC reclaims
orphaned objects according to the remaining roots.

An offline snapshot first makes the sealed layer durable, then prepares the next
upper generation, and finally publishes snapshot and image references in one
SQLite transaction. The previous upper remains usable before commit. GC can
reclaim interrupted, unreferenced outputs. If a commit's outcome is unknown,
query its state before retrying; do not blindly retry with a new name.

Online snapshots reuse upstream ImageFile snapshot/restack operations and retain
the active engine and lease. Publication intents and receipts are persistent.
Only operations with proven success can complete publication automatically;
ambiguous states continue to block conflicting access and GC. Online snapshots
do not pause applications or flush their caches and are not consistent backups
for arbitrary filesystems or applications.

Completion of an ordinary RAW lease write does not establish durability. The sync
path flushes upstream data and indexes and invokes macOS `F_FULLFSYNC`. Hybrid
in-place overwrites do not guarantee whole-block atomicity. Process-termination
tests do not replace power-loss validation.

## Leases and devices

Leases bind a persistent attachment ID, instance ID, random credential, and
generation, rather than the lifetime of a control connection. Stable mounted-data
callbacks do not depend on the control daemon staying alive. The engine holds the
actual `runtime.lock`. Normal unmounting synchronizes data, closes the engine, and
writes a close receipt. The controller verifies the lock and receipt before
releasing the lease.

An abnormal exit does not establish that a consumer is offline. Inspect
`obd leases` and `obd pending-restacks`, confirm that every consumer is detached,
then use `obd recover TOKEN --confirmed-offline`. Recovery must also acquire the
actual lock. It does not repair the inner filesystem, torn blocks, or an uncertain
restack.

Device creation, mounting, and deletion record persistent intents and verify the
FSKit source, DiskImages backing, and virtual-disk identity. Busy devices or
failed observations retain resources without forced unmounting. Formatting is
limited to a virtual disk created and verified by the current app or test flow.
Importing, branching, reopening, and recovery do not format existing data.

## Archives and space usage

Imports accept supported immutable sealed LSMT/ZFile layers and copy them into the
managed store. External mutable files are not hard-linked as lower layers. Export
publishes `image.json` last; a failed export directory is not a complete archive.
Compression and compaction reuse the pinned upstream implementation. Compatibility
is limited to the versions and formats actually validated, not arbitrary historical
implementations.

Space samples distinguish shared and exclusive layers, the current upper, file
lengths, and host-allocated bytes. Sampling is not atomic during active writes.
It does not infer free space inside APFS, APFS clone sharing, or the space that
deletion will release. See [HTTP CAS](http-cas.md) for remote transfers, integrity
checks, and offline behavior.
