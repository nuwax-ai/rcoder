#!/usr/bin/env python3
"""Verify the exact Pingap source pin and apply RCoder's reviewed protocol patch."""
import argparse
import hashlib
import json
import re
from pathlib import Path
import subprocess
import sys
import tomllib

HERE = Path(__file__).resolve().parent


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def checked_manifest(directory=HERE):
    directory = Path(directory)
    manifest = json.loads((directory / 'manifest.json').read_text())
    if manifest.get('protocol') != 'rcoder-pingap-applied-source-v1' or manifest.get('apply_protocol_version') != 1:
        raise ValueError('unsupported paired Pingap source manifest')
    if manifest.get('repository') != 'https://github.com/vicanso/pingap' or not re.fullmatch(r'[0-9a-f]{40}', manifest.get('base_commit', '')):
        raise ValueError('paired source must pin the official repository and full commit')
    patch = directory / manifest['patch_file']
    if patch.parent.resolve() != directory.resolve() or sha256(patch) != manifest['patch_sha256']:
        raise ValueError('paired Pingap patch SHA256 mismatch')
    return manifest, patch


def check_pair(root, manifest):
    root = Path(root).resolve()
    sys.path.insert(0, str(root / 'tools/build'))
    from pingap_identity import source_identity
    identity = source_identity(root)
    if identity['commit'] != manifest['base_commit'] or identity['version'] != manifest['version']:
        raise ValueError('paired patch disagrees with app-cli source identity')


def run(source, *args, capture=False):
    return subprocess.run(args, cwd=source, check=True,
                          text=True, capture_output=capture)


def verify_files(source, manifest):
    source = Path(source).resolve()
    for name, expected in manifest.get('source_files', manifest['patched_files']).items():
        relative = Path(name)
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('unsafe patched source path')
        path = source / relative
        if not path.is_file() or path.is_symlink() or sha256(path) != expected:
            raise ValueError('patched source SHA256 mismatch: ' + name)


def apply(source, directory=HERE, root=None, verify_only=False):
    source = Path(source).resolve()
    manifest, patch = checked_manifest(directory)
    if root is not None:
        check_pair(root, manifest)
    if (source / '.git').exists():
        actual = run(source, 'git', 'rev-parse', 'HEAD', capture=True).stdout.strip()
        if actual != manifest['base_commit']:
            raise ValueError('Pingap checkout must match exact base commit ' + manifest['base_commit'])
    elif verify_only and (source / 'SOURCE_MANIFEST.json').is_file():
        exported = json.loads((source / 'SOURCE_MANIFEST.json').read_text())
        if exported.get('base_commit') != manifest['base_commit'] or exported.get('patch_sha256') != manifest['patch_sha256']:
            raise ValueError('exported Pingap source disagrees with paired source identity')
    else:
        raise ValueError('source requires an exact-pin Git checkout or a verified frozen source export')
    if verify_only:
        verify_files(source, manifest)
        return manifest
    if run(source, 'git', 'status', '--porcelain', '--untracked-files=all', capture=True).stdout:
        raise ValueError('Pingap checkout must be clean before applying the reviewed patch')
    cargo = tomllib.loads((source / 'Cargo.toml').read_text())
    version = cargo.get('workspace', {}).get('package', {}).get('version', cargo['package'].get('version'))
    if version != manifest['version']:
        raise ValueError('Pingap source version disagrees with paired manifest')
    run(source, 'git', 'apply', '--check', str(patch))
    run(source, 'git', 'apply', str(patch))
    verify_files(source, manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--repo-root', type=Path)
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    try:
        manifest = apply(args.source, root=args.repo_root, verify_only=args.verify_only)
        print(json.dumps({key: manifest[key] for key in ('base_commit', 'patch_sha256', 'apply_protocol_version')}, sort_keys=True))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print('paired Pingap source: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
