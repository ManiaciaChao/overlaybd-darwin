#!/usr/bin/env python3
"""Reject ad-hoc/development signatures and device-restricted FSKit profiles."""
import datetime
import plistlib
from pathlib import Path
import subprocess
import sys

app = Path(sys.argv[1]).resolve()
source_root = Path(__file__).resolve().parents[1]
private_prefixes = [str(source_root).encode(), str(Path.home()).encode()]

extension = app / 'Contents/Extensions/OverlayBDFSKit.appex'
expected_tools = {'obd', 'overlaybd-macosd', 'obd-engine-worker'}
if {p.name for p in (app / 'Contents/Helpers').iterdir()} != expected_tools:
    raise SystemExit('Unexpected helper or test executable in App bundle')
for path in [app, extension, *(app / 'Contents/Helpers').iterdir()]:
    executable = path
    if path.is_dir():
        info = plistlib.loads((path / 'Contents/Info.plist').read_bytes())
        executable = path / 'Contents/MacOS' / info['CFBundleExecutable']
    binary = executable.read_bytes()
    if any(prefix in binary for prefix in private_prefixes):
        raise SystemExit(f'{path.name}: private build/home path found in executable')
    result = subprocess.run(['codesign', '-dvv', str(path)], capture_output=True, text=True, check=True)
    signature = result.stderr
    if 'Authority=Developer ID Application:' not in signature or 'Timestamp=' not in signature:
        raise SystemExit(f'{path.name}: requires Developer ID signing and secure timestamp')
    if 'runtime' not in signature:
        raise SystemExit(f'{path.name}: hardened runtime is required for notarization')
profile = extension / 'Contents/embedded.provisionprofile'
if not profile.is_file():
    raise SystemExit('FSKit extension requires an embedded Developer ID distribution profile')
result = subprocess.run(['security', 'cms', '-D', '-i', str(profile)], capture_output=True, check=True)
p = plistlib.loads(result.stdout)
e = p.get('Entitlements', {})
if not p.get('ProvisionsAllDevices') or p.get('ProvisionedDevices') or e.get('get-task-allow') or e.get('com.apple.security.get-task-allow'):
    raise SystemExit('FSKit profile is device-restricted/development; cannot distribute to colleagues')
if p.get('ExpirationDate', datetime.datetime.min) <= datetime.datetime.utcnow():
    raise SystemExit('FSKit provisioning profile has expired')
if not e.get('com.apple.developer.fskit.fsmodule'):
    raise SystemExit('FSKit entitlement missing from distribution profile')
print('Developer ID signatures, secure timestamps, and unrestricted FSKit profile verified')
