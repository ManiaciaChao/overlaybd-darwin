# Pinned-format interoperability fixture

This directory generates and validates synthetic 1 MiB images with 512-byte
logical blocks. Fixtures cover layered overwrites, zero writes, holes, and the
last sector. Scripts generate all data; no real disks, repositories, or user files
are included.

`fixtures.py` manages reference bytes and archive validation. `upstream-probe`
provides an independent reader using the pinned AgentENV revision.
`stage.py OUTPUT` stages the public upstream storage source and probe in a new
output directory and refuses to overwrite existing output.

Provide a Linux Rust/C++ toolchain compatible with the pinned upstream revision.
This is separate from the default local tests.

```sh
python3 tests/integration/interop/stage.py /tmp/obd-interop-stage
```

Write results and archives to external temporary directories, not the public
source tree. A successful merge command alone does not establish compatibility:
bring the output back and compare it against every reference byte.

Results apply only to the pinned revision and fixtures actually tested. They do
not establish compatibility with historical formats, arbitrary block sizes,
cross-platform APFS mounting, or all OCI configurations.
