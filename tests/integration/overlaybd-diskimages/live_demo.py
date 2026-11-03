#!/usr/bin/env python3
"""Live block snapshot/fork through the unmodified upstream restack algorithm.
Creates/erases only its own verified virtual disk. Does not freeze arbitrary apps.
Source remains mounted while snapshot, fork and branch writes take place.
"""
import fcntl
import json
from pathlib import Path
import signal
import subprocess
import tempfile
import time
from demo import REPO, raw


def main():
    root = Path(tempfile.mkdtemp(prefix='obd-live-', dir='/tmp'))
    print(f'Live demo artifacts: {root}', flush=True)
    ipc = root/'ipc'; ipc.mkdir(mode=0o700)
    log = (root/'daemon.log').open('w')
    daemon = subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'), str(root/'store'), str(ipc)], stdout=log, stderr=log)
    active = []
    success = False
    def cli(*args):
        return json.loads(raw.run(str(REPO/'target/debug/obd'), '--socket', str(ipc/'control.sock'), *args))
    def expose(image):
        endpoint = Path(cli('image', 'resource', image)['directory'])
        proxy = root/(image+'-proxy'); proxy.mkdir(exist_ok=True)
        volume = root/(image+'-volume'); volume.mkdir(exist_ok=True)
        raw.run('/sbin/mount', '-w', '-F', '-t', 'obdraw', str(endpoint), str(proxy))
        record = {'image':image, 'proxy':proxy, 'raw':proxy/'disk.raw', 'volume':volume, 'device':None}
        active.append(record)
        return record
    def attach(record, formatted=True):
        record['device'] = raw.attach(record['raw'], record['volume'] if formatted else None)
    def detach(record):
        if record['device'] is None: return
        deadline = time.monotonic()+20
        while True:
            try:
                raw.detach(record['raw'], record['device']); record['device'] = None; return
            except subprocess.CalledProcessError as e:
                if e.returncode != 16 or time.monotonic()>deadline: raise
                time.sleep(.5)
    def close(record):
        detach(record)
        raw.run('/sbin/umount', str(record['proxy']))
        active.remove(record)
    def write(record, contents):
        raw.write_verified(record['volume'], contents)
        with (record['volume']/'overlaybd-persistence-test.txt').open('rb') as f:
            # Darwin F_FULLFSYNC: drain this controlled test workload before
            # requesting a block snapshot. No general app-consistency claim.
            fcntl.fcntl(f.fileno(), 51)
    def check(record, contents):
        if (record['volume']/'overlaybd-persistence-test.txt').read_bytes() != contents:
            raise RuntimeError('live snapshot/fork content mismatch')
    try:
        deadline = time.monotonic()+15
        while not (ipc/'connection.json').exists():
            if daemon.poll() is not None or time.monotonic()>deadline: raise RuntimeError('daemon startup failed')
            time.sleep(.05)
        cli('image', 'create', 'source', '--size', '256M')
        source = expose('source'); attach(source, formatted=False)
        raw.verify_ownership(source['raw'], source['device'])
        raw.run('diskutil', 'eraseDisk', 'APFS', 'OverlayBD-Live', 'GPT', source['device'])
        detach(source); attach(source)
        write(source, b'before live snapshot\n')
        daemon.kill(); daemon.wait(timeout=15)
        write(source, b'before live snapshot\n')
        check(source, b'before live snapshot\n')
        print('Controller stopped: mounted APFS write/read/fullfsync succeeded', flush=True)
        daemon = subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'), str(root/'store'), str(ipc)], stdout=log, stderr=log)
        deadline=time.monotonic()+15
        while True:
            try:
                cli('image','list'); break
            except subprocess.CalledProcessError:
                if time.monotonic()>deadline: raise
                time.sleep(.1)
        # Give the independent control task time to re-prove its attachment.
        time.sleep(1)
        original_device = source['device']
        before = cli('leases')
        inode = source['raw'].stat().st_ino
        print('Source APFS remains mounted; invoking upstream live restack', flush=True)
        cli('snapshot', 'create', 'source', '--name', 'checkpoint', '--live')
        after = cli('leases')
        assert len(before)==len(after)==1
        assert before[0]['token']==after[0]['token']
        assert after[0]['generation']==before[0]['generation']+1
        assert source['device']==original_device and source['raw'].stat().st_ino==inode
        raw.verify_ownership(source['raw'], original_device)
        assert cli('pending-restacks')==[]
        write(source, b'source changed after snapshot\n')
        cli('image', 'fork', 'checkpoint', 'branch')
        branch = expose('branch'); attach(branch)
        check(branch, b'before live snapshot\n')
        write(branch, b'branch changed independently\n')
        check(source, b'source changed after snapshot\n')
        check(branch, b'branch changed independently\n')
        assert len(cli('leases'))==2
        print('Source and fork mounted together; independent contents verified', flush=True)
        close(branch); close(source)
        for image, contents in [('source',b'source changed after snapshot\n'),('branch',b'branch changed independently\n')]:
            item=expose(image); attach(item); check(item,contents); close(item)
        deadline=time.monotonic()+10
        while cli('leases'):
            if time.monotonic()>deadline: raise RuntimeError('leases remain after clean unmount')
            time.sleep(.1)
        assert cli('pending-restacks')==[]
        success=True
        print(json.dumps({'passed':True,'live_snapshot':True,'source_device_unchanged':True,'same_lease_token':True,'concurrent_apfs_fork':True,'reopen_isolation':True,'controller_restart_io':True,'store':str(root/'store')},indent=2),flush=True)
    finally:
        # Failed cleanup deliberately leaves daemon and backing store alive.
        for record in list(reversed(active)): close(record)
        daemon.send_signal(signal.SIGINT)
        daemon.wait(timeout=15); log.close()
        print(f'{"Completed" if success else "Failed"} live demo retained at {root}',flush=True)


if __name__=='__main__':
    try: main()
    except subprocess.CalledProcessError as e:
        raise SystemExit(f'{e.cmd} failed ({e.returncode}): {e.stderr.decode(errors="replace")}')
