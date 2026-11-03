#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/build-profile.sh
scripts/build-engine.sh
mkdir -p macos/build/module-cache
xcrun clang "${OBD_CLANG_FLAGS[@]}" -fobjc-arc -fmodules -fmodules-cache-path="$PWD/macos/build/module-cache" -mmacosx-version-min=26.0 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-deprecated-declarations -framework Foundation -framework FSKit macos/tests/RawVolumeTests.m "$PWD/target/$OBD_RUST_PROFILE/libobd_runtime.a" -framework Security -framework SystemConfiguration -liconv -lc++ -o macos/build/raw-volume-tests
macos/build/raw-volume-tests
