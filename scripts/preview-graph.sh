#!/bin/bash
# In-memory native UI fixture: no service, mounts, or user-library operations.
set -euo pipefail
cd "$(dirname "$0")/.."
APP="$PWD/macos/build/GraphPreview.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" macos/build/module-cache
cp -R macos/OverlayBDHost/en.lproj macos/OverlayBDHost/zh-Hans.lproj "$APP/Contents/Resources/"
python3 - <<'PY'
import plistlib
from pathlib import Path
Path('macos/build/GraphPreview.app/Contents/Info.plist').write_bytes(plistlib.dumps({
    'CFBundleIdentifier': 'dev.overlaybd.graphpreview', 'CFBundleExecutable': 'GraphPreview',
    'CFBundleName': 'GraphPreview', 'CFBundlePackageType': 'APPL',
    'NSPrincipalClass': 'NSApplication', 'LSMinimumSystemVersion': '26.0',
}))
PY
xcrun swiftc -target arm64-apple-macos26.0 -module-cache-path macos/build/module-cache \
    -parse-as-library macos/OverlayBDHost/Localization.swift macos/OverlayBDHost/Backend.swift macos/OverlayBDHost/LayerGraph.swift \
    macos/OverlayBDHost/LibraryModel.swift macos/OverlayBDHost/GraphViewportView.swift \
    macos/OverlayBDHost/RelationshipView.swift macos/tests/HostGraphPreview.swift \
    -o "$APP/Contents/MacOS/GraphPreview"
echo "Built memory-only UI fixture: $APP"
