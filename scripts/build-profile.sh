# Shared by the shell build/test entry points. Xcode supplies CONFIGURATION.
OBD_BUILD_CONFIGURATION="${CONFIGURATION:-${OBD_BUILD_CONFIGURATION:-Debug}}"
case "$OBD_BUILD_CONFIGURATION" in
    Debug) OBD_RUST_PROFILE=debug; OBD_CARGO_FLAGS=(--profile dev); OBD_CLANG_FLAGS=(-O0); OBD_SWIFT_FLAGS=(-Onone) ;;
    Release) OBD_RUST_PROFILE=release; OBD_CARGO_FLAGS=(--release); OBD_CLANG_FLAGS=(-O2); OBD_SWIFT_FLAGS=(-O) ;;
    *) echo "Unsupported build configuration: $OBD_BUILD_CONFIGURATION" >&2; exit 2 ;;
esac
export OBD_BUILD_CONFIGURATION

# Release binaries must not embed the maintainer's workspace or Cargo home.
# CARGO_ENCODED_RUSTFLAGS preserves path arguments containing spaces.
if [[ "$OBD_BUILD_CONFIGURATION" == Release ]]; then
    export CARGO_ENCODED_RUSTFLAGS
    CARGO_ENCODED_RUSTFLAGS=$(python3 - "$PWD" "${CARGO_HOME:-$HOME/.cargo}" <<'PY'
import os, shlex, sys
flags=os.environ.get('CARGO_ENCODED_RUSTFLAGS')
parts=flags.split('\x1f') if flags else shlex.split(os.environ.get('RUSTFLAGS',''))
parts += ['--remap-path-prefix='+sys.argv[1]+'=/src/overlaybd-darwin',
          '--remap-path-prefix='+sys.argv[2]+'=/cargo',
          '--remap-path-prefix='+os.path.expanduser('~')+'=/home/build']
print('\x1f'.join(parts), end='')
PY
)
    # Native dependencies may embed __FILE__ independently of Rust's remapper.
    export CFLAGS_SHELL_ESCAPED=1
    for variable in CFLAGS CXXFLAGS; do
        mapped=$(python3 - "$variable" "$PWD" "$HOME" <<'PYFLAGS'
import os, shlex, sys
flags=shlex.split(os.environ.get(sys.argv[1], ''))
flags += ['-ffile-prefix-map='+sys.argv[2]+'=/src/overlaybd-darwin',
          '-ffile-prefix-map='+sys.argv[3]+'=/home/build']
print(shlex.join(flags), end='')
PYFLAGS
)
        export "$variable=$mapped"
    done
    OBD_CLANG_FLAGS+=("-ffile-prefix-map=$PWD=/src/overlaybd-darwin")
    OBD_SWIFT_FLAGS+=(-enable-upcoming-feature ConciseMagicFile -debug-prefix-map "$PWD=/src/overlaybd-darwin" -file-prefix-map "$PWD=/src/overlaybd-darwin")
fi
