#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
source scripts/build-profile.sh
cargo build --offline --locked -p obd-runtime --lib "${OBD_CARGO_FLAGS[@]}"
