cask "overlaybd" do
  version "0.1.0"
  sha256 "9b2aaa896986bcc4e762145d4e144f5c1427ab5f692bd9488c5ea0e753a01e31"

  url "https://github.com/ManiaciaChao/overlaybd-darwin/releases/download/v#{version}/OverlayBD-#{version}-arm64.dmg"
  name "OverlayBD for macOS"
  desc "Layered virtual disks with snapshots, forks, and write compression"
  homepage "https://github.com/ManiaciaChao/overlaybd-darwin"

  depends_on arch: :arm64
  depends_on macos: :tahoe

  app "OverlayBD.app"

  # Also runs before an upgrade removes the previous app. Never force-unmount
  # volumes or terminate a service that may still own live storage.
  uninstall_preflight_steps do
    run "/bin/sh", args: ["-c", <<~'SH']
      set -eu
      mounts=$(/sbin/mount)
      case "$mounts" in
        *"(obdraw,"*|*"(obdraw)"*)
          echo "Safely unmount all OverlayBD images before upgrading or uninstalling." >&2
          exit 1
          ;;
      esac
      if /usr/bin/pgrep -x OverlayBDHost >/dev/null; then
        echo "Quit OverlayBD safely before upgrading or uninstalling." >&2
        exit 1
      else
        status=$?
        if [ "$status" -ne 1 ]; then
          echo "Unable to check whether OverlayBD is running; no app files were removed." >&2
          exit 1
        fi
      fi
    SH
  end

  caveats <<~EOS
    Open OverlayBD from your Applications folder and enable OverlayBDFSKit in
    System Settings > General > Login Items & Extensions > File System Extensions.

    Enable command-line tools in OverlayBD > Settings & Diagnostics, then add
    ~/.local/bin to PATH to use obd with the app's background service.

    Safely unmount all images and quit OverlayBD before upgrading or uninstalling.
    After an upgrade, check whether the file system extension needs enabling again.
    Uninstalling this cask keeps your images, snapshots, preferences, and CLI launcher.
    Disable command-line tools in the app before uninstalling to remove its launcher.
  EOS
end
