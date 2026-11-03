#!/usr/bin/env python3
"""Destructive ONLY to newly-created temp RAWs. Never accepts a device argument.
M0-A baseline: RAW -> DiskImages, format, persistent writes, clone identity.
With --through-fskit, mount each RAW through installed obdraw before attaching.
"""
import argparse
import errno
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import tempfile
import time


def run(*args, plist=False):
    result = subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return plistlib.loads(result.stdout) if plist else result.stdout.decode()


def attach(raw, mount=None, kernel=False):
    args = ['hdiutil', 'attach', '-plist', '-readwrite', '-imagekey', 'diskimage-class=CRawDiskImage']
    if kernel: args.append('-kernel')
    args += ['-mountpoint', str(mount)] if mount else ['-nomount']
    result = run(*args, str(raw), plist=True)
    devices = [v['dev-entry'] for v in result['system-entities'] if re.fullmatch(r'/dev/disk\d+', v.get('dev-entry', ''))]
    if not devices:
        raise RuntimeError('attach returned no whole disk; inspect hdiutil info for cleanup')
    # First whole disk is the backing disk; APFS may add a synthesized container.
    return devices[0]


def verify_ownership(raw, device):
    if not re.fullmatch(r'/dev/disk\d+', device):
        raise RuntimeError('not a whole disk')
    info = run('hdiutil', 'info', '-plist', plist=True)
    matching = [i for i in info.get('images', []) if Path(i.get('image-path', '')).resolve() == raw.resolve()]
    if len(matching) != 1 or device not in [e.get('dev-entry') for e in matching[0].get('system-entities', [])]:
        raise RuntimeError('DiskImages ownership changed; refusing device operation')
    disk = run('diskutil', 'info', '-plist', device, plist=True)
    if disk.get('WholeDisk') is not True or disk.get('VirtualOrPhysical') != 'Virtual':
        raise RuntimeError('device is not a verified whole virtual disk')


def detach(raw, device):
    verify_ownership(raw, device)
    # No force: all volumes must cleanly unmount before the device disappears.
    run('hdiutil', 'detach', device)


def write_verified(mount, contents):
    path = mount / 'overlaybd-persistence-test.txt'
    with path.open('wb') as f:
        f.write(contents)
        f.flush()
        os.fsync(f.fileno())
    if path.read_bytes() != contents:
        raise RuntimeError('write/read mismatch')


def experiment(profile, through_fskit):
    # Keep artifacts after a failed detach so no live backing file is unlinked.
    root = Path(tempfile.mkdtemp(prefix='obd-raw-', dir='/tmp'))
    attached = []
    proxies = []
    timings = {}
    success = False
    try:
        base = root/'base.raw'
        with base.open('xb') as f:
            f.truncate(256*1024*1024)
        def expose(raw, readonly=False):
            if not through_fskit:
                return raw
            point = root/(raw.stem+'-proxy'); point.mkdir()
            run('/sbin/mount', '-r' if readonly else '-w', '-F', '-t', 'obdraw', str(raw), str(point))
            proxies.append(point)
            return point/'disk.raw'
        if through_fskit:
            readonly_raw = expose(base, readonly=True)
            with readonly_raw.open('rb') as f:
                if f.read(512) != bytes(512):
                    raise RuntimeError('readonly mount read mismatch')
            try:
                fd = os.open(readonly_raw, os.O_WRONLY)
            except OSError as e:
                if e.errno not in (errno.EACCES, errno.EROFS):
                    raise
            else:
                os.close(fd)
                raise RuntimeError('readonly mount permitted writable open')
            point = proxies[-1]
            run('/sbin/umount', str(point)); proxies.pop(); point.rmdir()
        raw = expose(base)
        device = attach(raw); attached.append((raw,device))
        verify_ownership(raw,device)
        start=time.monotonic()
        run('diskutil','eraseDisk',profile,'OBD-'+root.name,'GPT',device)
        timings['format_seconds']=time.monotonic()-start
        detach(raw,device); attached.remove((raw,device))
        mount=root/'base-mount'; mount.mkdir()
        device=attach(raw,mount); attached.append((raw,device))
        write_verified(mount,b'base\n')
        detach(raw,device); attached.remove((raw,device))
        # Stop proxy before cloning backing bytes; enforce the full sync path.
        if through_fskit:
            run('/sbin/umount',str(proxies[-1])); proxies.pop()
        clones=[]
        for name in ['left','right']:
            clone=root/(name+'.raw'); shutil.copyfile(base,clone)
            exposed=expose(clone)
            mount=root/(name+'-mount'); mount.mkdir()
            start=time.monotonic(); device=attach(exposed,mount); attached.append((exposed,device))
            timings[name+'_attach_seconds']=time.monotonic()-start
            if (mount/'overlaybd-persistence-test.txt').read_bytes()!=b'base\n':
                raise RuntimeError('clone initial content mismatch')
            clones.append((exposed,device,mount))
        # Both same-identity clones remain attached while each is modified.
        for n,(_,_,mount) in enumerate(clones): write_verified(mount, f'branch-{n}\n'.encode())
        for n,(exposed,device,mount) in enumerate(clones):
            if (mount/'overlaybd-persistence-test.txt').read_bytes()!=f'branch-{n}\n'.encode():
                raise RuntimeError('concurrent clone identity/isolation failure')
            detach(exposed,device); attached.remove((exposed,device))
            device=attach(exposed,mount); attached.append((exposed,device))
            if (mount/'overlaybd-persistence-test.txt').read_bytes()!=f'branch-{n}\n'.encode():
                raise RuntimeError('reopen persistence failure')
            detach(exposed,device); attached.remove((exposed,device))
        success=True
        return {'profile':profile,'through_fskit':through_fskit,'passed':True,'timings':timings}
    finally:
        cleanup_ok = True
        for raw,device in reversed(attached):
            try: detach(raw,device)
            except Exception as e:
                cleanup_ok = False
                print(f'Cleanup required for {device}, backing {raw}: {e}',file=__import__('sys').stderr)
        for proxy in reversed(proxies):
            try: run('/sbin/umount',str(proxy))
            except Exception as e:
                cleanup_ok = False
                print(f'Cleanup required for {proxy}: {e}',file=__import__('sys').stderr)
        if success and cleanup_ok: shutil.rmtree(root)
        else: print(f'Failed experiment retained at {root}',file=__import__('sys').stderr)
        if not cleanup_ok: raise RuntimeError('Cleanup incomplete; backing artifacts retained')


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile',choices=['APFS','JHFS+'],required=True)
    parser.add_argument('--through-fskit',action='store_true')
    args=parser.parse_args()
    try: print(json.dumps(experiment(args.profile,args.through_fskit),indent=2))
    except subprocess.CalledProcessError as e:
        raise SystemExit(f'{e.cmd} failed ({e.returncode}): {e.stderr.decode(errors="replace")}')
