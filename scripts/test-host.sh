#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p macos/build/module-cache
python3 scripts/check-localizations.py
cp -R macos/OverlayBDHost/en.lproj macos/OverlayBDHost/zh-Hans.lproj macos/build/
xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
    -parse-as-library macos/OverlayBDHost/Localization.swift macos/tests/HostLocalizationTests.swift \
    -o macos/build/host-localization-tests
macos/build/host-localization-tests
xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
    -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/tests/HostBackendTests.swift \
    -o macos/build/host-backend-tests
macos/build/host-backend-tests
xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
    -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/OverlayBDHost/LayerGraph.swift macos/OverlayBDHost/LibraryModel.swift \
    macos/tests/HostStartupTests.swift -o macos/build/host-startup-tests
macos/build/host-startup-tests
xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
    -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/OverlayBDHost/LayerGraph.swift \
    macos/tests/HostGraphTests.swift -o macos/build/host-graph-tests
macos/build/host-graph-tests
if [[ "${OBD_TEST_STORE:-0}" == 1 ]]; then
    xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
        -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/OverlayBDHost/LayerGraph.swift \
        macos/OverlayBDHost/LibraryModel.swift macos/tests/HostGraphStoreTests.swift \
        -o macos/build/host-graph-store-tests
    macos/build/host-graph-store-tests
fi
if [[ "${OBD_TEST_STORAGE:-0}" == 1 ]]; then
    # Requires built CLI/daemon/worker tools, but no extension or disk mounts.
    xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
        -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/tests/HostStorageTests.swift \
        -o macos/build/host-storage-tests
    macos/build/host-storage-tests
fi
if [[ "${OBD_TEST_MOUNTS:-0}" == 1 ]]; then
    # Requires a signed exported App and an already enabled compatible extension.
    # No system permission changes, forced detach, or deletion of test stores.
    xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
        -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/tests/HostIntegrationTests.swift \
        -o macos/build/host-integration-tests
    macos/build/host-integration-tests
fi
