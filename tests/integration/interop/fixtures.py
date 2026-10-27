#!/usr/bin/env python3
"""Prepare macOS archives or verify independently produced upstream archives.
Small (1 MiB virtual) fixtures only; no filesystem mounts or user data.
"""
import argparse,json,subprocess,signal,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[3]
def main():
 p=argparse.ArgumentParser();p.add_argument('mode',choices=['prepare','verify','verify-cpp']);p.add_argument('root',type=Path);a=p.parse_args()
 root=a.root.resolve()
 marker=root/'.overlaybd-interop-fixture'
 if a.mode=='prepare':
  root.mkdir(exist_ok=True)
  if any(root.iterdir()):raise RuntimeError('prepare requires an empty test directory')
  marker.write_text('overlaybd-interop-fixture-1\n')
 elif not marker.is_file() or marker.read_text()!='overlaybd-interop-fixture-1\n':
  raise RuntimeError('verification requires a directory initialized by prepare')
 ipc=root/('ipc-'+a.mode);ipc.mkdir(mode=0o700)
 log=(root/('daemon-'+a.mode+'.log')).open('w')
 daemon=subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'),str(root/('store-'+a.mode)),str(ipc)],stdout=log,stderr=log)
 cli=[str(REPO/'target/debug/obd'),'--socket',str(ipc/'control.sock')]
 def call(*args):
  r=subprocess.run(cli+list(args),capture_output=True,text=True)
  if r.returncode:raise RuntimeError(r.stderr)
  return json.loads(r.stdout) if r.stdout.strip() else None
 report={'mode':a.mode,'passed':False}
 try:
  for _ in range(150):
   if daemon.poll() is not None:raise RuntimeError('daemon failed')
   try:call('image','list');break
   except RuntimeError:time.sleep(.1)
  else:raise RuntimeError('daemon timeout')
  if a.mode=='prepare':
   mac=root/'mac';mac.mkdir();size=1024*1024
   call('image','create','source','--size','1M')
   expected=bytearray(size);expected[:size//2]=b'\x35'*(size//2)
   payload=root/'payload';payload.write_bytes(expected[:size//2]);call('block','write','source','0','--input',str(payload))
   call('snapshot','create','source','--name','base')
   expected[:size//4]=b'\x73'*(size//4);expected[65536:131072]=b'\0'*65536;expected[-512:]=b'\xa5'*512
   payload.write_bytes(expected[:size//4]);call('block','write','source','0','--input',str(payload))
   payload.write_bytes(expected[-512:]);call('block','write','source',str(size-512),'--input',str(payload))
   call('snapshot','create','source','--name','top')
   call('snapshot','compact','top','--name','flat')
   (mac/'expected.raw').write_bytes(expected)
   report['before']=call('snapshot','stat','top');report['after']=call('snapshot','stat','flat')
   for snapshot in ['top','flat']:
    for codec in ['none','lz4','zstd']:
     call('snapshot','export',snapshot,'--output',str(mac/(snapshot+'-'+codec)),'--compression',codec)
  else:
   source=root/('cpp' if a.mode=='verify-cpp' else 'linux')
   expected_path=(root/'mac' if a.mode=='verify-cpp' else source)/'expected.raw'
   if expected_path.stat().st_size!=1024*1024:raise RuntimeError('expected a 1 MiB synthetic fixture')
   expected=expected_path.read_bytes()
   cases=sorted(p.parent.name for p in source.glob('case-*/image.json')) if a.mode=='verify-cpp' else ['none','lz4','zstd']
   assert cases
   for codec in cases:
    call('image','import',codec,'--image-config',str(source/codec/'image.json'))
    out=root/(codec+'.raw');call('block','read',codec,'0',str(len(expected)),'--output',str(out))
    assert out.read_bytes()==expected
    call('snapshot','create',codec,'--name',codec+'-snapshot')
    call('snapshot','compact',codec+'-snapshot','--name',codec+'-flat')
    call('image','fork',codec+'-flat',codec+'-branch')
    out.unlink();call('block','read',codec+'-branch','0',str(len(expected)),'--output',str(out))
    assert out.read_bytes()==expected
   report['upstream_to_macos']=cases;report['recompact_readback']=True
  report['passed']=True
 finally:
  daemon.send_signal(signal.SIGINT);daemon.wait(timeout=20);log.close()
  (root/(a.mode+'-report.json')).write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
if __name__=='__main__':main()
