# Homebrew installation

The project repository also serves as a custom Homebrew tap. No separate tap
repository is required. The cask installs the signed release app; it does not
compile the FSKit extension locally.

## Install

Requires Apple Silicon, macOS 26 (Tahoe) or later, and Homebrew.

```sh
brew tap ManiaciaChao/overlaybd-darwin https://github.com/ManiaciaChao/overlaybd-darwin.git
brew install --cask maniaciachao/overlaybd-darwin/overlaybd
```

The explicit tap URL is needed because this repository does not have the usual
`homebrew-` repository prefix. The app is installed as `OverlayBD.app` in
Homebrew's application directory, which defaults to `/Applications`.

Open the installed app and follow setup to enable OverlayBDFSKit in System
Settings. Homebrew installation does not grant permission to the extension.
If a manually installed copy already occupies the destination, safely quit it
and resolve the duplicate before installing. Do not use `--force` to replace an
app with mounted images or unresolved storage operations.

For the CLI, use **Settings & Diagnostics → Command Line** in the app. This
creates the managed `~/.local/bin/obd` launcher, which connects to the GUI service.
The cask does not install a second launcher or independent daemon.

## Upgrade and uninstall

Safely unmount every image and quit OverlayBD first. If the app refuses to quit,
resolve the pending operation or busy volume before continuing. The uninstall
preflight, also used during upgrades, refuses to remove the app when an obdraw
volume is mounted or the host app is running. It does not force-unmount volumes,
kill processes, or recover leases.

```sh
brew update
brew upgrade --cask maniaciachao/overlaybd-darwin/overlaybd
```

After upgrading, reopen the app and check the extension status. macOS may require
you to enable the updated extension again.

```sh
brew uninstall --cask maniaciachao/overlaybd-darwin/overlaybd
```

Uninstallation preserves the store, snapshots, preferences, and managed CLI
launcher. Disable the command-line tools in the app before uninstalling if you
also want to remove its launcher. The cask deliberately has no `zap` data-removal
rules. Keep only one installed copy of the host app for normal use.

## Maintain the cask

`Casks/overlaybd.rb` pins a published DMG and its SHA-256. For each new release:

1. Publish and verify the signed, notarized release DMG and `SHA256SUMS`.
2. Download both assets into a temporary directory and run
   `shasum -a 256 -c SHA256SUMS` there. Compare the result with the release asset
   digest when available. Never use `sha256 :no_check`.
3. Update the cask version and SHA-256 together. Check the app name, Apple Silicon
   architecture, minimum macOS version, and nested signature in the new artifact.
4. Validate the cask with Homebrew, then commit and push its update to this tap.

Check Ruby syntax locally without installing or replacing an app:

```sh
ruby -c Casks/overlaybd.rb
```

Once the update is available in your installed tap, check Homebrew's resolved
metadata with `brew info --cask maniaciachao/overlaybd-darwin/overlaybd`.
Current Homebrew requires casks to belong to a tap and rejects direct installation
or inspection of an arbitrary local `.rb` path.

Validate installation, FSKit enablement, mounting, safe upgrade refusal, and
uninstallation on an isolated Mac before claiming end-to-end support. Source
declarations and a valid download alone do not establish FSKit activation.
