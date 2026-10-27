#!/usr/bin/env python3
"""Test-only anonymous CAS server: conditional PUT and strict byte Range GET.
Binds loopback. Never use as a production authenticated distribution service.
"""
import hashlib, http.server, re, threading
from pathlib import Path

class CAS(http.server.ThreadingHTTPServer):
    def __init__(self, root):
        self.root=Path(root); self.root.mkdir(exist_ok=True)
        self.requests=[]; self.offline=False; self.corrupt=False; self.reject_put=False
        self.guard=threading.Lock()
        super().__init__(('127.0.0.1',0), Handler)
    @property
    def url(self): return f'http://127.0.0.1:{self.server_port}'

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def path_info(self):
        match=re.fullmatch(r'/(blobs|manifests)/(sha256:[0-9a-f]{64})', self.path)
        if not match: self.send_error(404); return None
        return self.server.root / (match[1]+'-'+match[2]), match[2]
    def do_PUT(self):
        if self.server.reject_put:self.send_error(503);return
        item=self.path_info()
        if item is None:return
        path,digest=item
        if self.headers.get('If-None-Match')!='*': self.send_error(428);return
        size=int(self.headers.get('Content-Length','0'))
        if size<=0 or size>64*1024*1024:self.send_error(413);return
        data=self.rfile.read(size)
        if 'sha256:'+hashlib.sha256(data).hexdigest()!=digest:self.send_error(422);return
        with self.server.guard:
            if path.exists(): status=412
            else:path.write_bytes(data);status=201
        self.send_response(status);self.send_header('Content-Length','0');self.end_headers()
    def do_GET(self): self.read_object(False)
    def do_HEAD(self): self.read_object(True)
    def read_object(self,head):
        if self.server.offline:self.send_error(503);return
        item=self.path_info()
        if item is None:return
        path,_=item
        if not path.exists():self.send_error(404);return
        data=path.read_bytes();size=len(data);start=0;end=size-1;status=200
        range_header=self.headers.get('Range')
        if range_header:
            m=re.fullmatch(r'bytes=(\d+)-(\d*)',range_header)
            if not m:self.send_error(416);return
            start=int(m[1]);end=min(int(m[2]) if m[2] else size-1,size-1)
            if start>end:self.send_error(416);return
            status=206
        payload=data[start:end+1]
        if self.server.corrupt and payload:payload=bytes([payload[0]^1])+payload[1:]
        self.server.requests.append({'path':self.path,'range':range_header,'bytes':0 if head else len(payload)})
        self.send_response(status);self.send_header('Content-Length',str(len(payload)))
        self.send_header('Accept-Ranges','bytes')
        if status==206:self.send_header('Content-Range',f'bytes {start}-{end}/{size}')
        self.end_headers()
        if not head:
            try:self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):pass

if __name__=='__main__':
    import argparse
    p=argparse.ArgumentParser();p.add_argument('directory',type=Path);a=p.parse_args()
    server=CAS(a.directory);print(server.url,flush=True);server.serve_forever()
