#!/usr/bin/env python3
"""Real layered image demo. Formats only its own verified temporary virtual disk.
Requires installed/enabled development FSKit extension and built Rust binaries.
Retains the store for inspection; never force-detaches or removes live backing.
"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('raw_verify', REPO/'tests/integration/raw-diskimages/verify.py')
raw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(raw)


def main():
    root = Path(tempfile.mkdtemp(prefix='obd-demo-', dir='/tmp'))
    print(f'Demo artifacts: {root}', flush=True)
    endpoint = root/'ipc'; endpoint.mkdir(mode=0o700)
    mount = root/'volume'; mount.mkdir()
    log = (root/'daemon.log').open('w')
    daemon = subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'), str(root/'store'), str(endpoint)], stdout=log, stderr=log)
    device = None; exposed = False; success = False
    disk = None
    def cli(*args):
        return raw.run(str(REPO/'target/debug/obd'), '--socket', str(endpoint/'control.sock'), *args)
    def expose(image):
        nonlocal exposed, device, disk
        # Persisted lifecycle is owned by the controller, including failures.
        exposed = True
        record = json.loads(cli('device', 'create', '1', 'overlaybd', '--image', image))
        device = record['device_path']
        disk = Path(record['proxy'])/'disk.raw'
        raw.verify_ownership(disk, device)
    def unexpose():
        nonlocal exposed, device
        deadline = time.monotonic()+20
        while True:
            try:
                cli('device', 'delete', '1')
                break
            except subprocess.CalledProcessError as error:
                if b'busy' not in (error.stderr or b'').lower() or time.monotonic()>deadline:
                    raise
                time.sleep(.5)
        exposed = False; device = None
    def attach():
        raw.verify_ownership(disk, device)
        cli('device', 'mount', '1', '--mountpoint', str(mount))
    def detach():
        # The controller handles guest unmount, DiskImages detach, then FSKit unmount.
        unexpose()
    def check(contents):
        if (mount/'overlaybd-persistence-test.txt').read_bytes() != contents:
            raise RuntimeError('layered image content mismatch')
    try:
        deadline = time.monotonic()+15
        while not (endpoint/'control.sock').exists():
            if daemon.poll() is not None or time.monotonic()>deadline:
                raise RuntimeError('daemon did not start; see daemon.log')
            time.sleep(.05)
        cli('image', 'create', 'base', '--size', '256M')
        print('Created OverlayBD base image; connecting FSKit', flush=True)
        expose('base')
        raw.verify_ownership(disk, device)
        volume_name = 'OBD-' + root.name
        raw.run('diskutil', 'eraseDisk', 'APFS', volume_name, 'GPT', device)
        detach(); expose('base'); attach()
        raw.write_verified(mount, b'OverlayBD base persisted\n')
        detach()
        print('APFS write/detach succeeded; snapshot and fork', flush=True)
        cli('snapshot', 'create', 'base', '--name', 'checkpoint')
        cli('image', 'fork', 'checkpoint', 'branch')
        expose('branch'); attach(); check(b'OverlayBD base persisted\n')
        raw.write_verified(mount, b'OverlayBD branch changed\n')
        detach()
        print('Branch inherited snapshot and accepted independent changes; reopening both images', flush=True)
        for image, contents in [('base', b'OverlayBD base persisted\n'), ('branch', b'OverlayBD branch changed\n')]:
            expose(image); attach(); check(contents); detach()
        success = True
        print(json.dumps({'passed': True, 'path': 'APFS -> DiskImages -> FSKit -> C ABI -> OverlayBD', 'snapshot_fork_isolation': True, 'store': str(root/'store'), 'images': json.loads(cli('image', 'list'))}, indent=2), flush=True)
    finally:
        # Keep daemon alive if cleanup fails; never drop a live device backend.
        if device is not None:
            detach()
        if exposed:
            unexpose()
        daemon.send_signal(signal.SIGINT)
        try: daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f'daemon did not stop; retained {root}')
        log.close()
        print(f'{"Completed" if success else "Failed"} demo retained at {root}', flush=True)


if __name__ == '__main__':
    try: main()
    except subprocess.CalledProcessError as e:
        raise SystemExit(f'{e.cmd} failed ({e.returncode}): {e.stderr.decode(errors="replace")}')
