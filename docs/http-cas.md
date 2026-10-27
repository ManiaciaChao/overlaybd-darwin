# HTTP(S) content-addressed repositories

## Publishing and server protocol

```sh
obd snapshot publish SNAP --repository URL --compression zstd
```

Compression options are `none`, `lz4`, and `zstd`; the default is `zstd`. Publishing
exports immutable layers, streams their SHA-256 digests, and PUTs them to
`URL/blobs/sha256:HEX`. After each upload or conflict, the client GETs the object
to verify it. Only after every layer succeeds does it publish
`URL/manifests/sha256:HEX`. The manifest's digest is computed over its exact bytes.
Identical content can be published repeatedly; the server must keep objects
immutable.

The server must support:

- `PUT` with `If-None-Match: *` and `Content-Length`: return 2xx for a new object
  and 412 if it already exists.
- `GET` for complete objects, and Range GET with correct 206, `Content-Range`, and
  `Content-Length` responses. Upstream also makes URL-resolution requests, so
  support `HEAD`.

The upload client does not follow redirects. The initial implementation does not
support URL usernames or passwords, query strings, fragments, or external
credentials. HTTPS uses reqwest/rustls certificate validation without disabling
verification. The integration fixture validates local HTTP.

This protocol is not the OCI Registry push API or an S3-signed upload API. Generic
C++ Registry client interoperability is not claimed.

## Pulling and runtime reads

`obd image pull ID --manifest URL` verifies the manifest's digest and structure,
reads layer metadata through upstream Range requests, checks each layer's capacity
and that the complete stack can open, then atomically publishes registry
references. The upper remains local. Imported logical sectors remain 512 bytes.
A successful pull does not mean that all remote data has been downloaded or fully
verified with SHA-256.

The read path is:

```text
FSKit → Rust ImageFile → upstream RegistryFsV2/FileCacheBackend → HTTP(S)
```

The control daemon handles lifecycle operations only. Runtime reads reuse upstream
ZFile validation and decompression, with a default 64 KiB cache refill and a 1 GiB
cache limit per current upper. There is no background download of the entire
image. Caches stay inside authorized resource directories and are independent for
each image/upper. Upstream eviction manages the limit; the combined caches for a
store are not guaranteed to stay below 1 GiB.

`image usage` reports `remote_object_bytes`, current-upper cache file lengths, and
allocated bytes from `st_blocks` separately. A sparse cache file's length is not
the amount downloaded. The existing `referenced_*` totals still exclude caches,
the database, and retired uppers. Remote object sizes do not count as local
allocated space. `materialized` indicates that a complete local layer exists;
it does not mean that an already-open runtime has switched its data source.

## Integrity and offline behavior

- ZFile applies upstream CRC checks to compressed blocks that are read. This is
  not SHA-256 identity verification of the whole object. Raw LSMT does not provide
  equivalent per-compressed-block CRC checks. The first complete cryptographic
  verification requires downloading the entire object.
- `image fetch ID` requires the target image to be detached and have no lease. It
  streams the download, verifies size and SHA-256, calls `fsync`, publishes the
  local files, and refreshes the target resource manifest. Failed temporary
  objects do not become local layers. After success, the image can be remounted
  offline and continue receiving writes in its local upper.
- Upstream still contacts the origin to resolve a URL whenever it opens a remote
  layer. **The disk cache does not guarantee offline remounting.** An already-open
  engine may be able to read cached data only. Missing cold data produces errors,
  not fabricated zeroes.
- Failed opens retain the existing conservative lease-recovery behavior. Stop
  the relevant consumers, restore connectivity, inspect `obd leases` for the
  token, and use `obd recover TOKEN --confirmed-offline`. Persistent leases from
  failed opens are not automatically erased.
- A live fork captures a block-level point in time; it does not freeze applications
  or APFS. The mount fixture calls macOS `F_FULLFSYNC` on its test file before
  forking. This does not establish consistency guarantees for arbitrary
  unsynchronized applications.
- Snapshots and live forks preserve remote references and independent uppers.
  Compaction, export, and publishing first materialize and verify required remote
  layers. These commands traverse data and are not metadata-only operations.

The registry uses an optional `remote_layers` field within the schema 1 database.
Older binaries reject unknown fields, so downgrading is unsupported.
`resource.json` remains version 1 and supports both local and remote layers.
Upstream source code is unchanged.

Offline `gc` can reclaim temporary downloads and transfer/probe directories left
by interrupted processes. Failed publishing may leave uploaded CAS objects with
no manifest references. Remote garbage collection is the repository's
responsibility; local cleanup does not delete remote shared objects.

## Validation

`tests/integration/http-cas/verify.py` covers 8 MiB of random data, Range reads,
idempotent publishing, offline recovery, digest-corruption rejection,
fork/compact/export/import, restart, and GC.

`mount_verify.py` performs a small real-FSKit/APFS test covering mounting, writing,
live forks, and offline remounting after fetching. The local test server,
`server.py`, limits each object to 64 MiB and is not intended for production or
large working sets.
