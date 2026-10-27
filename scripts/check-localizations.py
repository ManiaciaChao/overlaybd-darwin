#!/usr/bin/env python3
"""Check catalog coverage and interpolation without requiring the app to launch."""
from pathlib import Path
import json
import re

root = Path(__file__).resolve().parents[1]
host = root / "macos/OverlayBDHost"
pair = re.compile(r'("(?:[^"\\]|\\.)*")\s*=\s*("(?:[^"\\]|\\.)*");')
catalogs = {}
for language in ["en", "zh-Hans"]:
    pairs = [(json.loads(k), json.loads(v)) for k, v in pair.findall((host / f"{language}.lproj/Localizable.strings").read_text())]
    catalog = dict(pairs)
    assert len(catalog) == len(pairs), f"Duplicate keys in {language}"
    catalogs[language] = catalog
english, chinese = catalogs["en"], catalogs["zh-Hans"]
assert english.keys() == chinese.keys(), "Catalog keys do not match"
used = set()
for source in host.glob("*.swift"):
    for raw in re.findall(r'L10n\.tr\(("(?:[^"\\]|\\.)*")', source.read_text()):
        key = json.loads(raw)
        assert key in english, f"Missing translation in {source.name}: {key}"
        used.add(key)
for key in english:
    assert key == english[key], f"English fallback mismatch: {key}"
    assert key.count("%@") == chinese[key].count("%@"), f"Placeholder mismatch: {key}"
assert english.keys() == used, f"Unused catalog keys: {english.keys() - used}"
print(f"PASS: {len(english)} bilingual keys, source coverage, and matching placeholders")
