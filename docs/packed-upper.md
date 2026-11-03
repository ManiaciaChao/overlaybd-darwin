# Optional packed upper

This repository integrates the direct logical-block packed upper from the
[AgentENV fork](https://github.com/ManiaciaChao/AgentENV), pinned through the
`vendor/agentenv` submodule. See [upstream-revisions.md](upstream-revisions.md).
The native engine owns mapping, compression, dedupe, publication and reclamation;
this repository owns the adapter, store/runtime lifecycle, CLI, FSKit and GUI.
Packed is opt-in; existing creation defaults and legacy readers are preserved.

The following UI validation notes describe the original implementation history.
For validation of this integration, see [development.md](development.md).

## Mac UI controls (2026-11-02)

The create, local/remote import and snapshot-branch editors expose a future-write
compression picker: Off, LZ4 (recommended), or Zstd. Enabling explicitly selects
packed; the existing uncompressed creation default remains unchanged. For imports
and branches the policy is applied to the new local image, not its source layers.
Snapshots themselves do not have a mutable write policy.

An existing image's detail page displays the actual upper policy and opens an
editor for changing it. The daemon's `image usage` metadata now includes nullable
`write_policy` (`backend`, `compression`, `dedupe`); snapshots return null. Missing
policy metadata is shown as unavailable rather than interpreted as disabled.
The UI requires a detached, writable, initialized image, reobserves devices and
policy before applying, and preserves the current dedupe setting. The daemon
continues to enforce device/lease exclusion. Nothing automatically detaches a
mounted image. Enabling affects later writes and does not bulk-recompress data;
disabling keeps existing compressed data readable.

Validation: Swift typechecking and the host backend/startup/graph/compression
suites passed. Compression tests cover CLI backend selection, creation before
format, imports/branches, dedupe preservation, stale device observations, error
retention and unavailable/read-only policy guards. An isolated real-store test
uses the UI model through CLI/daemon for legacy migration, LZ4/Zstd/off and a
separate compressed branch, without FSKit mounts. The workspace passed **59 tests,
5 ignored**, including byte-preserving legacy-to-packed migration and policy
readback after restart. Release build and Clippy completed (existing warnings
remain). Creation/settings forms were checked in a temporary memory-only native
window. No new FSKit performance result is claimed.

Run `scripts/test-host.sh` for the fast host suites. Set `OBD_TEST_STORE=1` and
`OBD_TEST_TOOLS` to the freshly built helper directory for real-store tests;
`OBD_SWIFT_MODULE_CACHE` can select a fresh cache when using a copied workspace.

## UI storage measurements (2026-11-02)

Image details and the graph inspector offer on-demand storage sampling. The
display separates virtual capacity, logical mapped data, host allocated bytes,
exclusive bytes and upper payload compression. Samples include their time and
are invalidated when the image's generation or upper changes. Errors retain an
explicitly labeled prior result; absent/empty counters never imply a zero or
infinite compression ratio. Snapshots retain their existing physical usage view.

The detail overview uses aligned label/value rows, with a persistent compression
ratio row (for example `3.13×`), regardless of when compression was enabled.
First opening an image detail samples once if no cached measurement exists;
further changes use the refresh button. A compact two-column table includes an
inline compression-setting button; the separate future-write policy section is
removed. Accounting details are available in the info popover. The ratio remains
explicitly scoped to the current upper;
moving it into the overview does not turn it into a whole-image/lower-layer ratio.

- Logical data counts the current merged mapping, excluding zero masks, and
  includes plaintext writeback overlays without draining them. This is not guest
  filesystem used space; deleted files may remain mapped without guest TRIM.
- Host allocation retains the `st_blocks` accounting of the current upper and
  referenced lowers, including sharing and in-file retirement/metadata overhead.
  Cache and unreferenced historical files remain outside this number.
- Packed upper payload compression is **unique plaintext bytes / encoded bytes**.
  Savings are **1 - encoded / unique plaintext**. Dedupe does not multiply the
  numerator; zero elision, metadata, retired roots, lower layers and foreground
  unencoded buffers are excluded. Counters describe the coordinator's latest
  completed encoding batch, not a new durability acknowledgement. Legacy formats
  without payload counters show unavailable rather than an invented ratio.

`image storage-stats ID --refresh` requests a sample; subsequent calls without
`--refresh` retrieve a pending live result. A detached image is sampled by the
exclusive offline worker. With an attachment, the daemon requests metadata via
the existing authenticated control poll; the owning engine pins an I/O view and
samples outside its state mutex. No second upper is opened, no payload is sent
over IPC, and no cache drain, flush, barrier or registry publication is added by
sampling. Pending/results are volatile and bound to attachment proof, request,
generation and upper identity. Restack takes priority. Idle control polls keep
their old JSON shape, and sampling failures do not change I/O results.

Sampling can traverse mappings and competes for CPU/locks while explicitly
requested; it is not continuous polling and carries no new throughput claim.
Tests cover undrained cache overlays, dedupe/metadata exclusion, live sampling,
restack, daemon restart, offline fallback, empty payloads, stale samples and UI
failure retention. Validation: **61 workspace tests passed, 5 ignored; 445 native
tests passed, 2 ignored**; Swift host suites, isolated real-store UI/CLI sampling,
native UI layout inspection, Release build and Clippy completed.

## CLI and compatibility

```sh
obd image create workspace --size 2G --upper-backend packed
obd image create workspace-zstd --size 2G --upper-backend packed --compression zstd
obd image create workspace-dedupe --size 2G --upper-backend packed --dedupe on
# Detach before changing local policy:
obd image compression workspace none --upper-backend packed
obd image compression workspace lz4 --upper-backend packed --dedupe off
```

Packed defaults to LZ4 with dedupe off. A packed upper has `upper.packed` storage
and an `upper.policy` local configuration file. Policy changes affect future
writes; existing raw/LZ4/Zstd objects and shared references remain readable.
Changing an existing packed upper's policy does not create a snapshot. Migrating
from an older backend uses the existing standard-layer snapshot/replacement
workflow. There is no in-place upgrade of an older upper format.

Snapshot/export materializes a standard sealed LSMT layer. Snapshot/fork/restack
continue through the existing intent, receipt, and directory publication paths.
Export is not constant-time. Older readers consume the exported standard layer,
not the new writable packed file. New readers retain older backend support.

## I/O and durable publication

Foreground writes hold logical **4 KiB block range** leases, including partial
sector writes. Their RMW view includes pending writes, the committing generation,
the published upper, and then lower. Zero mappings mask lower data. Explicit
adapter discard supports packed; FSKit/DiskImages still do not forward guest
TRIM, so guest deletion does not promise immediate host reclamation.

A coordinator owns the mutable storage core. Foreground locks protect short
queue/map operations; compression, positional I/O and durability barriers execute
outside those locks. Reads pin an immutable working or durable root. Pending, prepared and inflight plaintext is bounded at 16 MiB; a coordinator timer wakes every 100 ms.
Explicit flush captures the prior-write sequence boundary and wakes the
coordinator immediately. It returns only after that boundary is durable.

A generation writes data and COW metadata, issues `F_FULLFSYNC`, publishes the
alternate checksummed superblock, then issues `F_FULLFSYNC` again. No successful
flush is acknowledged after a storage/sync failure. Data page writes coalesce
into contiguous batches up to 2 MiB. Large batches use a process-shared pool of at most four encoding workers.
Small batches avoid worker dispatch. The coordinator alone updates mappings.
Plaintext is copied into at most 128 KiB slabs and indexed in COW chunks. A block
view retains and charges its entire slab until its last owner releases it. The
same 16 MiB allocation ceiling includes recycled plaintext slabs; pressure forces
writeback even when sparse overwrites leave fewer logical dirty entries.
A preparation thread starts at 4 MiB of pending data and dispatches
encoding while the current batch is in flight. Pending, prepared, and in-flight
plaintext together remain bounded at 16 MiB; reads resolve them newest-first.
The process-wide encoder limit remains four. Workers pre-pack independent
objects, with the per-object allocator retained for fragmented free runs. A batch
can cross segment boundaries, padding only an object that would cross a boundary;
each individual object remains inside one 2 MiB segment. Less than one batch of
small free holes may be left for metadata and small writes. Larger free-space
amounts still force reuse, so this preference cannot accumulate unlimited holes.
The shared encoder output recycler retains at most eight 1 MiB buffers.
A bounded ordered data writer allows preparation to overlap positional writes.
Its queue holds at most 4 MiB of exact-sized encoded buffers, with individual
writes capped at 2 MiB and a separate reusable 2 MiB assembly buffer. Admission
uses bytes, not a fixed count of partially filled buffers;
encoding scratch and the metadata cache are separately bounded. Working reads
pin immutable queued buffers and read completed ranges from disk. Queued extents
remain reserved, including those beyond the current file EOF, until their writes
finish. Capacity writeback can therefore expose its working view without draining
the data queue. Durable publication always drains the queue before barrier one.
Reader pins may briefly retain completed encoded buffers until their copy ends.
Capacity writeback writes payload and coalesces metadata deltas in memory; durable
publication serializes the COW trees once. An ordered publisher thread executes
the two barriers and superblock write while the coordinator prepares the next
generation. For real files, a per-file physical-write mutex keeps the complete
two-barrier publication sequence separate from newer queued writes, so a full
sync does not chase a concurrently dirtied file. Preparation and reads continue;
the bounded queues apply backpressure if that disk phase takes a long time. Only
completion of barrier two advances the durable sequence.

The adapter bypasses its legacy `BufferedImage` for packed images and avoids
legacy upper.data/index flushes. Runtime I/O uses active-operation leases;
close and snapshot wait for those leases. FSKit reads/writes take shared engine
lifecycle leases and call Rust outside the Objective-C monitor. Flush waits for
prior operation tickets while allowing later I/O to proceed; both the runtime and
FSKit callback boundary enforce this fence, including multi-call FSKit writes.
Close still takes an exclusive lifecycle lease. A frozen generation is shared
between the frontend and core instead of cloning its dirty-block index.

## Background reclamation

The coordinator selects victims and prepares bounded relocation transactions
when foreground generations are idle. Cleaner root publication uses the ordered
publisher asynchronously. A separate worker punches safe ranges, each limited
to 2 MiB, with at most 16 MiB reserved per batch. Reserved pages remain unavailable
to allocation until successful punch/barrier completion is consumed. Foreground
flush does not wait for that completion. Invalid space must reach 8 MiB or 30%
of allocated page ranges before maintenance starts; reusable small holes do not
cause a separate full-sync barrier. Root rotations stop when only reader pins
retain old pages. Victim selection and relocation preparation still execute on
the coordinator and can yield only at transaction boundaries.

Both recoverable roots, an in-flight publication's frozen root, the working root
and outstanding read views protect their reachable pages. Queued data buffers
and active punch reservations are also excluded from allocation.
Root reachability reconstructs retirement after restart; the format does not
currently contain a separate persistent retirement-queue tree. COW disk metadata is incremental. Published in-memory mappings, objects and
page-reference counts now share 256-key chunks; only changed chunks are copied.
Protected page accounting uses shared bitmaps. Chunk directories and free-space
bitmap traversal still have volume-dependent cost. Encoded-byte totals and
cleaner fragmentation accounting are maintained incrementally.

Optional dedupe uses SHA-256 candidates followed by full plaintext comparison.
Candidate eviction affects hit rate, not object references. The latest logical
mapping and referenced objects are loaded in memory; the dirty-data bound is not
a bound on total engine memory.


## Acceptance scope

Earlier five-pair tests in the compression development workspace met the approved
median criterion for default LZ4 with dedupe off. They are historical results,
not performance acceptance of this merged public tree. Raw trials and tuning
archives remain in that development workspace; they are not copied into this
public source tree. New performance claims require a new same-machine FSKit run.
Strict per-4-KiB synchronous latency remains a separately reported metric.

Keep the backend opt-in. Total resident mapping memory and reopen cost are not
bounded by the 16 MiB plaintext budget. Reclamation selection and relocation
preparation can occupy the coordinator until transaction boundaries. Guest file
deletion does not guarantee immediate host-space release without forwarded TRIM.
