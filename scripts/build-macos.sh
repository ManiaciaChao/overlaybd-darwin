#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
BUILD="$PWD/macos/build"
source scripts/build-profile.sh
if [[ -n "${OBD_DEVELOPMENT_TEAM:-}" ]]; then
    # Device registration must be explicitly opted into by the account owner.
    provisioning=(-allowProvisioningUpdates)
    if [[ "${OBD_REGISTER_DEVICE:-0}" == 1 ]]; then
        provisioning+=(-allowProvisioningDeviceRegistration)
    fi
    exec xcodebuild -project macos/OverlayBD.xcodeproj -scheme OverlayBDHost \
        -configuration "$OBD_BUILD_CONFIGURATION" -derivedDataPath "$BUILD/xcode" \
        "${provisioning[@]}" "DEVELOPMENT_TEAM=$OBD_DEVELOPMENT_TEAM" build
fi
# The signed Xcode path builds Rust in its own build phase, once.
scripts/build-engine.sh
APP="$BUILD/OverlayBDHost.app"
EXT="$APP/Contents/Extensions/OverlayBDFSKit.appex"
# Replace only this script's generated ad-hoc bundle, avoiding stale PlugIns.
rm -rf "$APP"
mkdir -p "$BUILD/module-cache" "$EXT/Contents/MacOS" "$APP/Contents/MacOS"
xcrun clang "${OBD_CLANG_FLAGS[@]}" -fobjc-arc -fmodules -fmodules-cache-path="$BUILD/module-cache" -mmacosx-version-min=26.0 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-deprecated-declarations -c macos/OverlayBDFSKit/RawFileSystem.m -o "$BUILD/RawFileSystem.o"
xcrun swiftc "${OBD_SWIFT_FLAGS[@]}" -target arm64-apple-macos26.0 -module-cache-path "$BUILD/module-cache" -parse-as-library -import-objc-header macos/OverlayBDFSKit/RawFileSystem.h macos/OverlayBDFSKit/Extension.swift "$BUILD/RawFileSystem.o" "$PWD/target/$OBD_RUST_PROFILE/libobd_runtime.a" -framework Security -framework SystemConfiguration -liconv -lc++ -framework FSKit -framework Foundation -o "$EXT/Contents/MacOS/OverlayBDFSKit"
xcrun swiftc "${OBD_SWIFT_FLAGS[@]}" -target arm64-apple-macos26.0 -module-cache-path "$BUILD/module-cache" -parse-as-library macos/OverlayBDHost/*.swift -o "$APP/Contents/MacOS/OverlayBDHost"
cp macos/OverlayBDFSKit/Info.plist "$EXT/Contents/Info.plist"
cp macos/OverlayBDHost/Info.plist "$APP/Contents/Info.plist"
mkdir -p "$APP/Contents/Resources"
cp -R macos/OverlayBDHost/en.lproj macos/OverlayBDHost/zh-Hans.lproj "$APP/Contents/Resources/"
xcrun actool macos/OverlayBDHost/AppIcon.icon \
    --compile "$APP/Contents/Resources" --platform macosx \
    --minimum-deployment-target 26.0 --app-icon AppIcon \
    --output-partial-info-plist "$BUILD/app-icon-info.plist"
/usr/libexec/PlistBuddy -c "Merge $BUILD/app-icon-info.plist" "$APP/Contents/Info.plist"
OBD_APP_PATH="$APP" scripts/bundle-tools.sh
# Compilation artifact only. Real development signing uses Xcode above.
codesign --force --sign - --entitlements macos/OverlayBDFSKit/Extension.entitlements "$EXT"
codesign --force --sign - --entitlements macos/OverlayBDHost/Host.entitlements "$APP"
codesign --verify --deep --strict "$APP"
echo "Built $APP (ad-hoc compilation artifact; use OBD_DEVELOPMENT_TEAM for installation)"
