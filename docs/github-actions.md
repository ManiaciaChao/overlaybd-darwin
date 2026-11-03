# GitHub Actions

The public repository uses the standard ARM64 `xcode-27` GitHub-hosted image.
Standard hosted compute is free for public repositories; artifact/cache storage
has separate allowances. The image supplies Xcode 27 and a macOS SDK >= 26.
Rust is pinned to 1.98.1. Action dependencies are pinned to commit SHAs.

## CI

`macOS CI` runs on pull requests, pushes to `main`, and manual dispatch. It checks
source hygiene/localization/shell syntax, builds and tests the Rust workspace,
runs FSKit callback tests and Host tests (including isolated real stores), then
compiles the Release Xcode app without distribution credentials. It does not
activate extensions or mount disks. The optional store-relocation test also stays
local: its system-disk guard requires 20 GiB free beyond the store copy, exceeding
the standard runner's advertised disk space. Credentials are not available to this job.
Existing Rust formatting differences are not a CI gate; formatting cleanup can
be reviewed separately.

Pushes and pull requests that change only root `README*` files, `docs/**`, or
`Casks/**` skip `macOS CI`. A change to any other path, including source code,
build scripts, dependencies, or workflows, runs the full checks. Manual dispatch
and release calls to the reusable workflow always run the checks regardless of
the changed paths. No commit-message skip marker is needed for documentation or
cask-only updates.

## One-time release setup

In Settings → Environments create `release`. Under deployment branches and tags,
choose selected branches/tags and allow tags matching `v*`. An optional required
reviewer can approve each signing run. Store the following in this environment,
not in source files or chat:

| Kind | Name | Value |
|---|---|---|
| Variable | `APPLE_TEAM_ID` | Apple Developer Team ID |
| Secret | `APPLE_CERTIFICATE_BASE64` | Base64 of a password-protected `.p12` export containing exactly one Developer ID Application certificate **and its private key** |
| Secret | `APPLE_CERTIFICATE_PASSWORD` | Password of that `.p12` |
| Secret | `APPLE_FSKIT_PROFILE_BASE64` | Base64 of the Developer ID distribution `.provisionprofile` for `dev.overlaybd.mac.RawFS` |
| Secret | `APPLE_ID` | Apple account email authorized for notarization under this team |
| Secret | `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password generated in the Apple account; not the account login password |

Export the identity from Keychain Access → My Certificates → Developer ID
Application → Export as `.p12`. A certificate exported without its private key
cannot sign. Download the FSKit **Developer ID distribution** profile from the
Apple Developer account: it must contain `com.apple.developer.fskit.fsmodule`,
match this team and bundle ID, and permit all devices. A development profile with
registered-device restrictions is rejected. The host currently needs no profile
because its entitlements are empty.

You can upload values locally with GitHub CLI (substitute paths and IDs):

```sh
repo=ManiaciaChao/overlaybd-darwin
gh variable set APPLE_TEAM_ID --env release --repo "$repo" --body YOUR_TEAM_ID
base64 -i /path/to/DeveloperID.p12 | gh secret set APPLE_CERTIFICATE_BASE64 --env release --repo "$repo"
gh secret set APPLE_CERTIFICATE_PASSWORD --env release --repo "$repo"
base64 -i /path/to/FSKit.provisionprofile | gh secret set APPLE_FSKIT_PROFILE_BASE64 --env release --repo "$repo"
gh secret set APPLE_ID --env release --repo "$repo"
gh secret set APPLE_APP_SPECIFIC_PASSWORD --env release --repo "$repo"
```

Commands without piped input prompt for the value; do not place
passwords in shell command arguments. No App Store Connect API key or Xcode account
login is required for this workflow. Secrets are only passed to the signing step
on the GitHub-hosted release runner. The runner imports a temporary keychain and
profile, validates the profile, configures notarytool, and cleans up after the job.
GitHub also discards the hosted VM at the end of the job.

## Create a release

1. Merge the intended source into `main` and update both Info.plist versions and
   the version/build settings in `scripts/generate-xcode-project.py` together.
2. Create and push a new `vVERSION` tag on that commit (e.g. `v0.1.0` for version
   `0.1.0`). Only tag a version when you intend to build a release.
3. `macOS release` reruns CI, verifies that the tag is on `main` and matches both
   bundle versions, then signs, exports, notarizes, and staples a DMG. A failed
   signature/profile/notarization check prevents publishing the package.
4. Download the verified DMG from the run or the **draft** GitHub Release, test
   installation/extension activation/mounts on a Mac, then publish the draft.

The workflow uploads only the DMG and `SHA256SUMS`, retaining run artifacts for
seven days. Archives, profiles, keys, and notarization logs are not uploaded.
Creating a GitHub Release draft uses the built-in `GITHUB_TOKEN`; no PAT is needed.

To retry before the draft exists, dispatch `macOS release` with an existing tag
selected, or use `gh workflow run release.yml --ref vVERSION`. Dispatching from
`main` skips the release jobs. An existing release/draft is never overwritten;
if one already exists, retain it and inspect the new run artifact instead.

## Local packaging

The default local `package-dmg.sh` path continues to use automatic Xcode signing.
CI sets `OBD_DISTRIBUTION_PROFILE_PATH` to the imported distribution profile. It
builds an unsigned archive, embeds the profile, and signs helpers, extension, and
host from inside out with `codesign`, using the imported certificate fingerprint.
This supports Xcode-managed distribution profiles without an Xcode account login.
The actual signing certificate must be authorized by the profile. Signed Release
helpers request secure timestamps, as required by the distribution verifier.
Tag runs skip the extra unsigned Release compile because the packaging job builds
the Release archive itself. `OBD_NOTARY_KEYCHAIN` explicitly selects the temporary
keychain for notarization credentials.

See Apple's [distribution signing workflow](https://developer.apple.com/documentation/xcode/creating-distribution-signed-code-for-the-mac/).

References: [GitHub billing](https://docs.github.com/en/billing/concepts/product-billing/github-actions),
[runner images](https://github.com/actions/runner-images),
[Apple certificates in Actions](https://docs.github.com/en/actions/how-tos/deploy/deploy-to-third-party-platforms/sign-xcode-applications),
[Apple notarization credentials](https://developer.apple.com/documentation/technotes/tn3147-migrating-to-the-latest-notarization-tool).
