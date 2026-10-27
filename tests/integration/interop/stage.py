#!/usr/bin/env python3
"""Stage only public upstream storage sources and the independent format probe.
The offline/full-feature-disabled probe never needs the macOS workspace in Docker.
"""
import argparse,shutil
from pathlib import Path
repo=Path(__file__).resolve().parents[3]
p=argparse.ArgumentParser(description=__doc__);p.add_argument('output',type=Path);a=p.parse_args()
a.output.mkdir()  # never overwrite an existing staging directory
for name in ['overlaybd','util']:
    source=repo/'vendor/agentenv/storage'/name;dest=a.output/'storage'/name
    shutil.copytree(source/'src',dest/'src')
    manifest=(source/'Cargo.toml').read_text().split('[dev-dependencies]')[0]
    if name=='overlaybd':
        begin=manifest.index('default =');end=manifest.index('io-uring =',begin)
        manifest=manifest[:begin]+'default = []\nfull = []\n'+manifest[end:]
        manifest=manifest.split('# Optional (feature "full")')[0]
    (dest/'Cargo.toml').write_text(manifest)
shutil.copytree(repo/'tests/integration/interop/upstream-probe/src',a.output/'probe/src')
manifest=(repo/'tests/integration/interop/upstream-probe/Cargo.toml').read_text().replace('../../../../vendor/agentenv/storage/overlaybd','../storage/overlaybd')
(a.output/'probe/Cargo.toml').write_text(manifest)

shutil.copyfile(repo/'vendor/agentenv/LICENSE', a.output/'LICENSE-AgentENV')
shutil.copyfile(repo/'LICENSE', a.output/'LICENSE')
