#!/bin/bash
# GitHub-hosted release runners only. Never print credential contents.
set -euo pipefail
: "${RUNNER_TEMP:?}" "${GITHUB_ENV:?}"
: "${APPLE_TEAM_ID:?Set release environment variable APPLE_TEAM_ID}"
: "${APPLE_CERTIFICATE_BASE64:?}" "${APPLE_CERTIFICATE_PASSWORD:?}"
: "${APPLE_FSKIT_PROFILE_BASE64:?}" "${APPLE_ID:?}" "${APPLE_APP_SPECIFIC_PASSWORD:?}"
umask 077
credentials="$RUNNER_TEMP/overlaybd-credentials"
keychain="$RUNNER_TEMP/overlaybd-signing.keychain-db"
mkdir -p "$credentials"
trap 'rm -rf "$credentials"' EXIT
printf '%s' "$APPLE_CERTIFICATE_BASE64" | base64 --decode > "$credentials/certificate.p12"
printf '%s' "$APPLE_FSKIT_PROFILE_BASE64" | base64 --decode > "$credentials/fskit.provisionprofile"
security cms -D -i "$credentials/fskit.provisionprofile" > "$credentials/profile.plist"
profile_uuid=$(python3 - "$credentials/profile.plist" <<'PY'
import datetime, os, plistlib, sys, uuid
p = plistlib.load(open(sys.argv[1], 'rb'))
e = p.get('Entitlements', {})
team = os.environ['APPLE_TEAM_ID']
if p.get('TeamIdentifier') != [team] or e.get('com.apple.application-identifier') != team + '.dev.overlaybd.mac.RawFS':
    raise SystemExit('FSKit profile must match APPLE_TEAM_ID and dev.overlaybd.mac.RawFS')
if not p.get('ProvisionsAllDevices') or p.get('ProvisionedDevices') or e.get('get-task-allow') or e.get('com.apple.security.get-task-allow'):
    raise SystemExit('FSKit profile must be an unrestricted Developer ID distribution profile')
if not e.get('com.apple.developer.fskit.fsmodule') or p['ExpirationDate'] <= datetime.datetime.utcnow():
    raise SystemExit('FSKit profile is expired or lacks the FSKit entitlement')
# Xcode matches profile UUIDs case-sensitively. Validate without normalizing.
uuid.UUID(p['UUID'])
print(p['UUID'])
PY
)
profile_dir="$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"
profile_path="$profile_dir/$profile_uuid.provisionprofile"
mkdir -p "$profile_dir"
cp "$credentials/fskit.provisionprofile" "$profile_path"
# Record the profile path immediately so cleanup also works on partial failure.
printf 'OBD_CI_PROFILE_PATH=%s\n' "$profile_path" >> "$GITHUB_ENV"
keychain_password=$(openssl rand -hex 32)
printf '::add-mask::%s\n' "$keychain_password"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 7200 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$credentials/certificate.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" > /dev/null
security list-keychains -d user -s "$keychain" "$HOME/Library/Keychains/login.keychain-db"
security default-keychain -d user -s "$keychain"
identity=$(security find-identity -v -p codesigning "$keychain" | python3 -c '
import os, re, sys
matches = re.findall(r"\b([A-Fa-f0-9]{40})\s+\"Developer ID Application: [^\"]+ \(" + re.escape(os.environ["APPLE_TEAM_ID"]) + r"\)\"", sys.stdin.read())
if len(matches) != 1:
    raise SystemExit("Import exactly one valid Developer ID Application identity for APPLE_TEAM_ID, including its private key")
print(matches[0])
')
xcrun notarytool store-credentials overlaybd-ci --keychain "$keychain" \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD"
{
    printf 'OBD_DEVELOPMENT_TEAM=%s\n' "$APPLE_TEAM_ID"
    printf 'OBD_DISTRIBUTION_PROFILE_PATH=%s\n' "$profile_path"
    printf 'OBD_SIGN_IDENTITY=%s\n' "$identity"
    printf 'OBD_NOTARY_PROFILE=overlaybd-ci\n'
    printf 'OBD_NOTARY_KEYCHAIN=%s\n' "$keychain"
} >> "$GITHUB_ENV"
