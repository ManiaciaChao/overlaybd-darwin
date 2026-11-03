#!/usr/bin/env python3
"""Sign an unsigned Xcode archive using an existing Developer ID profile.

Apple's distribution workflow permits embedding distribution profiles and signing
nested code directly with codesign. This also supports Xcode-managed profiles
without requiring an Xcode account on the runner.
"""
import datetime
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
app = Path(sys.argv[1]).resolve()
extension = app / 'Contents/Extensions/OverlayBDFSKit.appex'
profile_path = Path(os.environ['OBD_DISTRIBUTION_PROFILE_PATH'])
team = os.environ['OBD_DEVELOPMENT_TEAM']
identity = os.environ.get('OBD_SIGN_IDENTITY', 'Developer ID Application')
profile = plistlib.loads(subprocess.check_output(['security', 'cms', '-D', '-i', str(profile_path)]))
allowed = profile.get('Entitlements', {})
if profile.get('TeamIdentifier') != [team] or allowed.get('com.apple.application-identifier') != team + '.dev.overlaybd.mac.RawFS':
    raise SystemExit('Distribution profile does not match the team and FSKit bundle ID')
if (not profile.get('ProvisionsAllDevices') or profile.get('ProvisionedDevices')
        or allowed.get('get-task-allow') or allowed.get('com.apple.security.get-task-allow')
        or not allowed.get('com.apple.developer.fskit.fsmodule')
        or profile.get('ExpirationDate', datetime.datetime.min) <= datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None)):
    raise SystemExit('An unexpired, unrestricted FSKit Developer ID profile is required')
for bundle, identifier in [(app, 'dev.overlaybd.mac'), (extension, 'dev.overlaybd.mac.RawFS')]:
    if plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())['CFBundleIdentifier'] != identifier:
        raise SystemExit('Unexpected distribution bundle identifier')
helpers = app / 'Contents/Helpers'
if {p.name for p in helpers.iterdir()} != {'obd', 'overlaybd-macosd', 'obd-engine-worker'}:
    raise SystemExit('Unexpected helper set')
shutil.copyfile(profile_path, extension / 'Contents/embedded.provisionprofile')
with tempfile.TemporaryDirectory(prefix='overlaybd-sign-') as temporary:
    entitlements = plistlib.loads((root / 'macos/OverlayBDFSKit/Extension.entitlements').read_bytes())
    for key in ['com.apple.application-identifier', 'com.apple.developer.team-identifier']:
        entitlements[key] = allowed[key]
    entitlement_path = Path(temporary) / 'extension.plist'
    entitlement_path.write_bytes(plistlib.dumps(entitlements))
    def sign(path, extra=()):
        subprocess.run(['codesign', '--force', '--sign', identity, '--options', 'runtime',
                        '--timestamp', *extra, str(path)], check=True)
    for helper in sorted(helpers.iterdir()):
        sign(helper)
    sign(extension, ['--entitlements', str(entitlement_path)])
    # Check the actual leaf certificate, rather than only matching its Team ID.
    certificate_prefix = str(Path(temporary) / 'certificate')
    subprocess.run(['codesign', '--display', '--extract-certificates=' + certificate_prefix,
                    str(extension)], check=True)
    leaf = Path(certificate_prefix + '0').read_bytes()
    if leaf not in profile.get('DeveloperCertificates', []):
        raise SystemExit('Signing certificate is not authorized by the FSKit profile')
    sign(app, ['--entitlements', str(root / 'macos/OverlayBDHost/Host.entitlements')])
subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
print('Developer ID signatures and profile certificate binding verified')
