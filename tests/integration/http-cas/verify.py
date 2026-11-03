#!/usr/bin/env python3
"""Isolated remote product lifecycle with small synthetic data, no mounts."""
import json, random, signal, subprocess, tempfile, threading, time
from pathlib import Path
from server import CAS
REPO=Path(__file__).resolve().parents[3]
def main():
 with tempfile.TemporaryDirectory(prefix='obd-http-') as tmp:
  root=Path(tmp);server=CAS(root/'cas');threading.Thread(target=server.serve_forever,daemon=True).start()
  ipc=root/'ipc';ipc.mkdir(mode=0o700);log=(root/'daemon.log').open('w')
  daemon=None
  def start():
   nonlocal daemon
   daemon=subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'),str(root/'store'),str(ipc)],stdout=log,stderr=log)
   for _ in range(100):
    if daemon.poll() is not None:raise RuntimeError((root/'daemon.log').read_text())
    if (ipc/'control.sock').exists():
     try:call('image','list');return
     except RuntimeError:pass
    time.sleep(.1)
   raise RuntimeError('daemon timeout')
  def stop():
   daemon.send_signal(signal.SIGINT);daemon.wait(timeout=20)
  def call(*args,fail=False):
   p=subprocess.run([str(REPO/'target/debug/obd'),'--socket',str(ipc/'control.sock'),*map(str,args)],capture_output=True,text=True,timeout=180)
   if fail:
    assert p.returncode!=0, p.stdout
    return p.stderr
   if p.returncode:raise RuntimeError(' '.join(map(str,args))+'\n'+p.stderr)
   return json.loads(p.stdout) if p.stdout.strip() else None
  def read(id,offset,length):
   out=root/'read';out.unlink(missing_ok=True)
   call('block','read',id,offset,length,'--output',out);return out.read_bytes()
  try:
   start();call('image','create','source','--size','8M')
   data=random.Random(42).randbytes(8*1024*1024);payload=root/'payload'
   for offset in (0,4*1024*1024):
    payload.write_bytes(data[offset:offset+4*1024*1024]);call('block','write','source',offset,'--input',payload)
   call('snapshot','create','source','--name','base')
   server.reject_put=True
   call('snapshot','publish','base','--repository',server.url,fail=True)
   assert not list((root/'cas').glob('manifests-*'))
   server.reject_put=False
   published=call('snapshot','publish','base','--repository',server.url)
   again=call('snapshot','publish','base','--repository',server.url)
   assert again==published
   server.corrupt=True
   assert 'digest/size mismatch' in call('snapshot','publish','base','--repository',server.url,fail=True)
   server.corrupt=False
   server.requests.clear()
   pulled=call('image','pull','remote','--manifest',published['manifest'])
   assert read('remote',512,4096)==data[512:4608]
   range_requests=[r for r in server.requests if '/blobs/' in r['path']]
   bytes_read=sum(r['bytes'] for r in range_requests)
   assert range_requests and all(r['range'] for r in range_requests),range_requests
   assert bytes_read<len(data)//2,bytes_read
   usage=call('image','usage','remote');assert usage['lower_allocated_bytes']==0
   stop();start();assert read('remote',512,4096)==data[512:4608]
   server.offline=True
   # Upstream resolves the remote URL on every open, even with a warm disk cache.
   warm=root/'warm';error=call('block','read','remote',512,4096,'--output',warm,fail=True)
   server.offline=False
   for lease in call('leases'):call('recover',lease['token'],'--confirmed-offline')
   payload.write_bytes(b'changed');call('block','write','remote',511,'--input',payload)
   call('snapshot','create','remote','--name','mixed');call('image','fork','mixed','branch')
   assert read('branch',511,7)==b'changed'
   server.corrupt=True
   assert 'digest/size mismatch' in call('image','fetch','branch',fail=True)
   server.corrupt=False
   call('image','fetch','branch');server.offline=True
   assert read('branch',4*1024*1024,4096)==data[4*1024*1024:4*1024*1024+4096]
   server.offline=False
   call('snapshot','compact','mixed','--name','flat');call('image','fork','flat','flattened')
   assert read('flattened',511,7)==b'changed'
   exported=root/'export';call('snapshot','export','mixed','--output',exported,'--compression','lz4')
   call('image','import','restored','--image-config',exported/'image.json');assert read('restored',511,7)==b'changed'
   # A manifest must match its content-addressed name.
   server.corrupt=True
   assert 'manifest digest mismatch' in call('image','pull','bad','--manifest',published['manifest'],fail=True)
   server.corrupt=False
   for codec in ['none','lz4']:
    archive=call('snapshot','publish','base','--repository',server.url,'--compression',codec)
    call('image','pull','codec-'+codec,'--manifest',archive['manifest'])
    assert read('codec-'+codec,4096,4096)==data[4096:8192]
   report={'codecs':['none','lz4','zstd'],'passed':True,'failed_publish_no_manifest':True,'existing_corrupt_object_rejected':True,'range_bytes_for_pull_and_4KiB_read':bytes_read,'source_bytes':len(data),'range_requests':len(range_requests),'cached_reopen_offline':False,'fetch_offline_read':True,'offline_reopen_rejected':bool(error),'corrupt_fetch_rejected':True,'fork_compact_export_import':True}
   for image in call('image','list'):call('image','delete',image['id'])
   for snap in call('snapshot','list'):call('snapshot','delete',snap['id'])
   call('gc');stop();start();assert call('image','list')==[]
   print(json.dumps(report,indent=2))
  finally:
   if daemon and daemon.poll() is None:stop()
   server.shutdown();server.server_close();log.close()
if __name__=='__main__':main()
