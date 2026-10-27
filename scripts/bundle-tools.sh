#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
source scripts/build-profile.sh
cargo build --offline --locked -p obd-cli -p obd-daemon -p obd-runtime --bin obd --bin overlaybd-macosd --bin obd-engine-worker "${OBD_CARGO_FLAGS[@]}"
APP="${OBD_APP_PATH:-${TARGET_BUILD_DIR:?}/${WRAPPER_NAME:?}}"
mkdir -p "$APP/Contents/Helpers"
for tool in obd overlaybd-macosd obd-engine-worker; do
    cp "target/$OBD_RUST_PROFILE/$tool" "$APP/Contents/Helpers/$tool"
    # These standalone Rust executables aren't separately signed by Xcode.
    identity="${EXPANDED_CODE_SIGN_IDENTITY:--}"
    if [[ "${CODE_SIGNING_ALLOWED:-YES}" == NO ]]; then identity=-; fi
    codesign --force --sign "$identity" --options runtime --timestamp=none "$APP/Contents/Helpers/$tool"
done

mkdir -p "$APP/Contents/Resources/Licenses"
cp LICENSE THIRD_PARTY_NOTICES "$APP/Contents/Resources/Licenses/"
cp docs/dependency-licenses.json "$APP/Contents/Resources/Licenses/"
ditto licenses/third-party "$APP/Contents/Resources/Licenses/third-party"
