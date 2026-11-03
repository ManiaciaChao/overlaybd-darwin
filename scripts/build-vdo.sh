#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/build-profile.sh
vdo_source="${OBD_VDO_SOURCE:-$PWD/../vdo-darwin}"
if [[ ! -f "$vdo_source/darwin/worker.c" ]]; then
    echo "Darwin VDO port missing: $vdo_source (set OBD_VDO_SOURCE)" >&2
    exit 1
fi
vdo_flags=(--worker)
if [[ "$OBD_RUST_PROFILE" == release ]]; then vdo_flags+=(--release); fi
python3 "$vdo_source/darwin/build.py" "${vdo_flags[@]}"
mkdir -p "target/$OBD_RUST_PROFILE"
cp "$vdo_source/darwin/build/obd-vdo-worker" "target/$OBD_RUST_PROFILE/obd-vdo-worker"
echo "Built target/$OBD_RUST_PROFILE/obd-vdo-worker"
