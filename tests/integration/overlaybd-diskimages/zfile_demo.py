#!/usr/bin/env python3
"""Small real APFS -> ZFile export/import -> FSKit mount roundtrip.
Formats only its own newly created, ownership-verified virtual device.
"""
import hashlib
import importlib.util
import json
from pathlib import Path
import signal
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('raw', REPO / 'tests/integration/raw-diskimages/verify.py')
raw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(raw)


def main():
    root = Path(tempfile.mkdtemp(prefix='obd-zfile-', dir='/tmp'))
    (root / 'ipc').mkdir(mode=0o700)
    point = root / 'volume'
    point.mkdir()
    log = (root / 'daemon.log').open('w')
    daemon = subprocess.Popen([str(REPO / 'target/debug/overlaybd-macosd'), str(root / 'store'), str(root / 'ipc')], stdout=log, stderr=log)
    cli = [str(REPO / 'target/debug/obd'), '--socket', str(root / 'ipc/control.sock')]

    def call(*args):
        return json.loads(raw.run(*cli, *args))

    def attach(name):
        record = call('device', 'create', '71', 'overlaybd', '--image', name)
        raw.verify_ownership(Path(record['proxy']) / 'disk.raw', record['device_path'])
        return record

    def mount():
        volumes = call('device', 'status', '71')[0]['observed']['volumes']
        apfs = [v for v in volumes if v['filesystem'] == 'apfs']
        assert len(apfs) == 1
        call('device', 'mount', '71', '--mountpoint', str(point), '--volume', apfs[0]['device_path'])

    report = {'root': str(root), 'passed': False, 'codecs': {}}
    try:
        for _ in range(150):
            if daemon.poll() is not None:
                raise RuntimeError('daemon exited')
            try:
                call('image', 'list')
                break
            except subprocess.CalledProcessError:
                time.sleep(.1)
        else:
            raise RuntimeError('daemon not ready')
        call('image', 'create', 'source', '--size', '256M')
        record = attach('source')
        raw.run('diskutil', 'eraseDisk', 'APFS', 'OBD-ZFile-Test', 'GPT', record['device_path'])
        call('device', 'unmount', '71')
        mount()
        payload = bytes(range(256)) * 4096
        raw.write_verified(point, payload)
        call('device', 'delete', '71')
        call('snapshot', 'create', 'source', '--name', 'base')
        attach('source')
        mount()
        payload = bytes(reversed(range(256))) * 4096
        raw.write_verified(point, payload)
        call('device', 'delete', '71')
        call('snapshot', 'create', 'source', '--name', 'archive')
        report['before_compact'] = call('snapshot', 'stat', 'archive')
        call('snapshot', 'compact', 'archive', '--name', 'flat')
        report['after_compact'] = call('snapshot', 'stat', 'flat')
        assert report['before_compact']['layer_count'] == 2
        assert report['after_compact']['layer_count'] == 1
        for codec in ['lz4', 'zstd']:
            export = root / codec
            start = time.monotonic()
            call('snapshot', 'export', 'flat', '--output', str(export), '--compression', codec)
            elapsed = time.monotonic() - start
            call('image', 'import', codec, '--image-config', str(export / 'image.json'))
            attach(codec)
            mount()
            actual = (point / 'overlaybd-persistence-test.txt').read_bytes()
            assert actual == payload
            raw.write_verified(point, b'independent writable upper\n')
            call('device', 'delete', '71')
            attach(codec)
            mount()
            assert (point / 'overlaybd-persistence-test.txt').read_bytes() == b'independent writable upper\n'
            call('device', 'delete', '71')
            report['codecs'][codec] = {'export_seconds': elapsed, 'archive_bytes': sum(p.stat().st_size for p in export.iterdir()), 'sha256': hashlib.sha256(actual).hexdigest(), 'write_and_remount': True}
        attach('source')
        mount()
        assert (point / 'overlaybd-persistence-test.txt').read_bytes() == payload
        call('device', 'delete', '71')
        report['source_unchanged'] = True
        report['passed'] = True
    finally:
        # No force: retain controller and backing if an active volume is busy.
        for record in call('device', 'status'):
            call('device', 'delete', str(record['dev_id']))
        assert call('leases') == []
        daemon.send_signal(signal.SIGINT)
        daemon.wait(timeout=20)
        log.close()
        result = REPO / 'macos/build/test-results/zfile-lifecycle.json'
        result.parent.mkdir(parents=True, exist_ok=True)
        result.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    main()
