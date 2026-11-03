#!/usr/bin/env python3
"""Update this project's development app while retaining ExtensionKit directory identity.
Run only after all obdraw mounts are detached. Never changes system security settings.
"""
import argparse
import os
import plistlib
from pathlib import Path
import shutil
import signal
import subprocess
import time


def run(*args):
    return subprocess.check_output(args, text=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    args = parser.parse_args()
    source = args.source.resolve()
    destination = Path.home()/'Applications/OverlayBDHost-Development.app'
    if source == destination.resolve() or not (source/'Contents/Extensions/OverlayBDFSKit.appex').is_dir():
        parser.error('source must be a separate built OverlayBDHost.app')
    if destination.is_symlink():
        parser.error('development app destination must not be a symlink')
    with (source/'Contents/Info.plist').open('rb') as f:
        if plistlib.load(f).get('CFBundleIdentifier') != 'dev.overlaybd.mac':
            parser.error('source is not the OverlayBD development host')
    run('codesign', '--verify', '--deep', '--strict', str(source))
    if 'obdraw' in run('mount'):
        raise RuntimeError('unmount all obdraw volumes before replacing the extension')
    executable = str(destination/'Contents/Extensions/OverlayBDFSKit.appex/Contents/MacOS/OverlayBDFSKit')
    def own_processes():
        result = []
        for line in run('ps', '-axo', 'pid=,command=').splitlines():
            parts = line.strip().split(None, 1)
            if len(parts) == 2 and (parts[1] == executable or parts[1].startswith(executable+' ')):
                result.append(int(parts[0]))
        return result
    for pid in own_processes():
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic()+10
    while own_processes():
        if time.monotonic() > deadline:
            raise RuntimeError('old unmounted extension did not exit; no files changed')
        time.sleep(.1)
    # Preserve app/appex directories: deleting them can invalidate the user's
    # ExtensionKit enablement bookmark. Remove only obsolete bundle resources.
    if destination.exists():
        for current, dirs, files in os.walk(destination, topdown=False, followlinks=False):
            for name in files+dirs:
                path = Path(current)/name
                original = source/path.relative_to(destination)
                if not original.exists() and not original.is_symlink():
                    if path.is_dir() and not path.is_symlink():
                        shutil.rmtree(path)
                    else:
                        path.unlink()
    run('ditto', str(source), str(destination))
    run('codesign', '--verify', '--deep', '--strict', str(destination))
    run('/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister', '-f', str(destination))
    run('pluginkit', '-a', str(destination/'Contents/Extensions/OverlayBDFSKit.appex'))
    print(f'Installed and verified {destination}')


if __name__ == '__main__':
    main()
