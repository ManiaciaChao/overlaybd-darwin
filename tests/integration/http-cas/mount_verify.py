#!/usr/bin/env python3
"""Real APFS/FSKit remote lazy read, local write, live fork and offline fetch.
Formats only its own newly created, ownership-verified virtual disk.
"""
import fcntl,importlib.util,json,random,shutil,signal,subprocess,tempfile,threading,time
from pathlib import Path
from server import CAS
REPO=Path(__file__).resolve().parents[3]
spec=importlib.util.spec_from_file_location('raw',REPO/'tests/integration/raw-diskimages/verify.py')
raw=importlib.util.module_from_spec(spec);spec.loader.exec_module(raw)
def main():
 root=Path(tempfile.mkdtemp(prefix='obd-http-mount-',dir='/tmp'));(root/'ipc').mkdir(mode=0o700)
 server=CAS(root/'cas');threading.Thread(target=server.serve_forever,daemon=True).start()
 point=root/'volume';point.mkdir();log=(root/'daemon.log').open('w')
 daemon=subprocess.Popen([str(REPO/'target/debug/overlaybd-macosd'),str(root/'store'),str(root/'ipc')],stdout=log,stderr=log)
 def call(*args):
  attempts=10 if args[:2]==('device','delete') else 1
  for attempt in range(attempts):
   try:return json.loads(raw.run(str(REPO/'target/debug/obd'),'--socket',str(root/'ipc/control.sock'),*map(str,args)))
   except subprocess.CalledProcessError as error:
    if attempt+1==attempts:raise RuntimeError(error.stderr.decode()) from error
    time.sleep(.5) # APFS/DiskImages teardown can transiently retain open handles; never force.

 def attach(name):
  record=call('device','create','72','overlaybd','--image',name)
  raw.verify_ownership(Path(record['proxy'])/'disk.raw',record['device_path']);return record
 def mount():
  vs=call('device','status','72')[0]['observed']['volumes'];volumes=[v for v in vs if v['filesystem']=='apfs'];assert len(volumes)==1
  call('device','mount','72','--mountpoint',point,'--volume',volumes[0]['device_path'])
 report={'passed':False}
 try:
  for _ in range(150):
   if daemon.poll() is not None:raise RuntimeError('daemon exited')
   try:call('image','list');break
   except (subprocess.CalledProcessError,RuntimeError):time.sleep(.1)
  call('image','create','source','--size','256M');record=attach('source')
  raw.run('diskutil','eraseDisk','APFS','OBD-HTTP-Test','GPT',record['device_path'])
  call('device','unmount','72');mount()
  payload=random.Random(2710).randbytes(8*1024*1024);raw.write_verified(point,payload)
  call('device','delete','72');call('snapshot','create','source','--name','base')
  pub=call('snapshot','publish','base','--repository',server.url)
  server.requests.clear();call('image','pull','remote','--manifest',pub['manifest'])
  attach('remote');mount()
  file=point/'overlaybd-persistence-test.txt'
  with file.open('rb') as f:assert f.read(4096)==payload[:4096]
  report['bytes_before_full_file_read']=sum(r['bytes'] for r in server.requests if '/blobs/' in r['path'])
  assert file.read_bytes()==payload
  raw.write_verified(point,b'remote local upper\n')
  with file.open('rb') as f:fcntl.fcntl(f.fileno(),51) # macOS F_FULLFSYNC, checkpoint before live fork
  call('image','fork','remote','branch','--live')
  raw.write_verified(point,b'parent after fork\n');call('device','delete','72')
  attach('branch');mount();actual=file.read_bytes();assert actual==b'remote local upper\n', (len(actual),actual[:64]);call('device','delete','72')
  call('image','fetch','branch');server.offline=True
  attach('branch');mount();assert file.read_bytes()==b'remote local upper\n';raw.write_verified(point,b'offline branch write\n');call('device','delete','72')
  server.offline=False
  attach('remote');mount();assert file.read_bytes()==b'parent after fork\n';call('device','delete','72')
  report.update(passed=True,apfs_virtual_bytes=256*1024*1024,payload_bytes=len(payload),remote_mount=True,live_fork_isolation=True,fetch_offline_mount_write=True)
 finally:
  for record in call('device','status'):call('device','delete',record['dev_id'])
  assert call('leases')==[]
  daemon.send_signal(signal.SIGINT);daemon.wait(timeout=20);log.close();server.shutdown();server.server_close()
  if report['passed']:shutil.rmtree(root)
  else:report['retained_debug_root']=str(root)
  output=REPO/'macos/build/test-results/http-cas-mount.json';output.parent.mkdir(parents=True,exist_ok=True)
  output.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
if __name__=='__main__':main()
