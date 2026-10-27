#!/usr/bin/env python3
"""Fail on private build paths, common credentials and unexpected release files.

Third-party license attribution and the public upstream submodule are preserved.
Findings print filenames and rule names only, never a matched credential.
"""
from pathlib import Path
import json
import os
import re
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
rules = {
    'private home path': r'/Users/(?!USER(?:/|\b))[^/\s"`]+',
    'private volume path': r'(?<!/System)/Volumes/[^\s"`]+',
    'absolute Swift file literal': r'#filePath\b',
    'private key': r'-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----',
    'GitHub credential': r'\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})',
    'AWS access key': r'\b(?:AKIA|ASIA)[A-Z0-9]{16}\b',
}
private_markers = [m for m in os.environ.get("OBD_PRIVATE_MARKERS", "").split(";") if m]
findings = []
files = subprocess.check_output(
    ['git', '-C', str(root), 'ls-files', '--cached', '--others', '--exclude-standard', '-z']
).decode().split('\0')
for name in files:
    if not name or name.startswith('vendor/'):
        continue
    rel = Path(name)
    p = root / rel
    if not p.is_file() or p == Path(__file__).resolve():
        continue
    if p.suffix.lower() in {'.p12', '.pfx', '.pem', '.key', '.provisionprofile', '.mobileprovision', '.dmg'} or p.name.startswith('.env'):
        findings.append((str(rel), 'unexpected credential/build file'))
    try:
        text = p.read_text()
    except UnicodeError:
        findings.append((str(rel), 'unexpected binary file'))
        continue
    for label, pattern in rules.items():
        if re.search(pattern, text):
            findings.append((str(rel), label))
    if any(m.casefold() in text.casefold() for m in private_markers):
        findings.append((str(rel), 'private identity marker'))
    if p.suffix == '.json':
        try:
            json.loads(text)
        except ValueError:
            findings.append((str(rel), 'invalid JSON after redaction'))
# Local histories and remotes are checked independently of source text.
if (root / '.git').exists() and subprocess.run(
    ['git', '-C', str(root), 'rev-parse', '--verify', 'HEAD'], capture_output=True
).returncode == 0:
    history = subprocess.check_output(['git', '-C', str(root), 'log', '--all', '--format=%an <%ae>'], text=True)
    if any(m.casefold() in history.casefold() for m in private_markers):
        findings.append(('.git history', 'private author identity'))
for path, rule in findings:
    print(f'{path}: {rule}')
print(f'Public-source audit: {len(findings)} finding(s)')
sys.exit(bool(findings))
