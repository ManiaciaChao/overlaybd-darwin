# Testing

Public tests use synthetic data, temporary stores, and processes started by the
tests themselves. Device paths, invalid credentials, and attack inputs are explicit
synthetic fixtures, not data from personal machines or production accounts. Test
executables are not included in the DMG.

## Default regressions

```sh
cargo build --locked --workspace
cargo test --locked --workspace
./scripts/test-macos.sh
./scripts/test-host.sh
```

| Test | Coverage | System operations |
|---|---|---|
| `HostLocalizationTests.swift` and `check-localizations.py` | System language matching, English fallback, saved preferences, bilingual coverage and interpolation | Isolated temporary preferences; no changes to the user's language settings |
| `crates/*/tests` and Rust unit tests | Block I/O, isolation, snapshots, leases, persistence, and crash boundaries | Private temporary directories and local sockets; fault injection terminates only test-owned daemons |
| `macos/tests/RawVolumeTests.m` | FSKit callbacks, boundaries, read-only access, and sync errors | Direct callback calls; no disk mounts |
| `HostBackendTests.swift` | CLI launcher protection, capacity and system-reserve boundaries, volume detection, formatting ownership checks, malformed observations, and process output handling | Synthetic device information and test subprocesses; no `/dev` access |
| `HostStartupTests.swift` | Permission gates, restore intent, failure isolation, safe shutdown, quick branching, mount naming, legacy origins, and space checks before creation | Mock backend; no login-item registration, mounts, or user-library changes |
| `HostGraphTests.swift` | Layer relationships, branches and snapshot aliases, navigation, long-chain folding, and shared-space deduplication | Synthetic in-memory graphs; no store operations |

Version rejection, invalid credentials, path escapes, and read-only enforcement
remain part of the regression suite. Fixture URLs use example.com, invalid test
domains, or local loopback addresses.

## Optional integration validation

### Store relocation

This test uses the built CLI, daemon, and worker in `target/debug` to verify image
and snapshot migration, branch data integrity, retention of the original copy,
persistent location selection, and protection against missing disks. It uses
only temporary unformatted images and does not mount disks.

```sh
OBD_TEST_STORAGE=1 ./scripts/test-host.sh
```

Set `OBD_TEST_TOOLS` to use another directory containing all three tools.

### Relationship graph and real storage

```sh
OBD_TEST_STORE=1 ./scripts/test-host.sh
```

This additionally uses real tools to compare the graph against actual layer
ordering, nested branches, snapshot deletion, and space usage. It uses only
temporary, unmounted images.

### HTTP CAS without mounts

The HTTP CAS test starts a temporary server listening on loopback and uses
deterministically generated data:

```sh
python3 tests/integration/http-cas/verify.py
```

### Real mounts

Install and enable the FSKit extension from **the build being tested** first.
These tests format only their own newly created disks after verifying the
DiskImages backing and whole virtual-disk identity. They do not accept arbitrary
physical device input.

```sh
OBD_TEST_MOUNTS=1 ./scripts/test-host.sh
python3 tests/integration/raw-diskimages/verify.py --profile APFS --through-fskit
python3 tests/integration/overlaybd-diskimages/demo.py
python3 tests/integration/overlaybd-diskimages/live_demo.py
python3 tests/integration/overlaybd-diskimages/zfile_demo.py
python3 tests/integration/http-cas/mount_verify.py
```

Use isolated, reproducible test data. On failure, retain backing data and any
necessary services for inspection. Tests do not force unmounts, automatically
release uncertain leases, or delete backing data that may still be mounted.
Successful mount tests may also retain detached temporary stores. Printed
temporary paths are for local investigation, not committed test results.

### Pinned-format interoperability

The [interoperability fixture](integration/interop/README.md) uses small synthetic
images and requires a separate Linux/Rust/C++ toolchain. The default local tests
do not automatically fetch, build, or run external tools.
