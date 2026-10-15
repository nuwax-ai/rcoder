#!/usr/bin/env python3
"""Stable Cargo source digest; generated target files never invalidate image builds."""
import hashlib
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
paths = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=root).split(b'\0')
digest = hashlib.sha256()
for raw in sorted(set(paths)):
    if not raw:
        continue
    path = Path(raw.decode())
    if path.parts[0] != 'crates' and str(path) not in {'Cargo.toml', 'Cargo.lock'}:
        continue
    if path.suffix != '.rs' and path.name not in {'Cargo.toml', 'Cargo.lock'}:
        continue
    digest.update(raw + b'\0')
    full = root / path
    digest.update(full.read_bytes() if full.is_file() else b'<missing>')
print(digest.hexdigest())
