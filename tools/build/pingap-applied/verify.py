#!/usr/bin/env python3
"""Verify a final image's Pingap against its paired source and binary receipt."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import subprocess
import sys


def read_object(path):
    value = json.loads(Path(path).read_text(encoding='utf-8'))
    if not isinstance(value, dict):
        raise ValueError('expected a JSON object: ' + str(path))
    return value


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def probe(binary, argument, expected):
    result = subprocess.run([str(binary), argument], text=True, encoding='utf-8',
                            capture_output=True, timeout=5)
    if result.returncode != 0 or result.stdout.strip() != expected:
        raise ValueError(f'Pingap {argument} failed final-image verification '
                         f'(exit={result.returncode}, expected={expected!r})')


def runtime_target():
    machine = platform.machine().lower()
    arch = {'amd64': 'x86_64', 'x86_64': 'x86_64',
            'arm64': 'aarch64', 'aarch64': 'aarch64'}.get(machine)
    suffix = {'linux': 'unknown-linux-gnu', 'darwin': 'apple-darwin',
              'win32': 'pc-windows-msvc'}.get(sys.platform)
    if arch is None or suffix is None:
        raise ValueError('unsupported paired Pingap runtime platform')
    return arch + '-' + suffix


def verify(binary, receipt_path, manifest_path, *, version=None, commit=None,
           require_full=False, tls=None):
    binary = Path(binary).resolve(strict=True)
    manifest = read_object(manifest_path)
    receipt = read_object(receipt_path)
    if (manifest.get('protocol') != 'rcoder-pingap-applied-source-v1'
            or manifest.get('apply_protocol_version') != 1
            or manifest.get('repository') != 'https://github.com/vicanso/pingap'
            or not re.fullmatch(r'[0-9a-f]{40}', str(manifest.get('base_commit', '')))
            or not re.fullmatch(r'[0-9a-f]{64}', str(manifest.get('patch_sha256', '')))
            or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', str(manifest.get('version', '')))):
        raise ValueError('invalid paired Pingap source manifest')
    if version is not None and version != manifest['version']:
        raise ValueError('image Pingap version disagrees with paired source')
    if commit is not None and commit != manifest['base_commit']:
        raise ValueError('image Pingap commit disagrees with paired source')
    expected = {
        'protocol': 'rcoder-pingap-applied-binary-v1',
        'apply_protocol_version': 1,
        'upstream_repository': manifest['repository'],
        'upstream_base_commit': manifest['base_commit'],
        'upstream_version': manifest['version'],
        'patch_sha256': manifest['patch_sha256'],
    }
    for field, value in expected.items():
        if receipt.get(field) != value:
            raise ValueError('Pingap binary receipt disagrees with paired source: ' + field)
    if receipt.get('target') != runtime_target():
        raise ValueError('Pingap binary receipt target disagrees with final runtime platform')
    checksum = digest(binary)
    if receipt.get('binary_sha256') != checksum:
        raise ValueError('final Pingap binary SHA256 disagrees with its receipt')
    backend = receipt.get('tls_backend')
    features = receipt.get('cargo_features')
    if backend not in ('openssl', 'rustls') or (tls is not None and backend != tls):
        raise ValueError('final Pingap TLS backend disagrees with image contract')
    tls_feature = 'openssl' if backend == 'openssl' else 'tls-rustls'
    if (not isinstance(features, list) or tls_feature not in features
            or (require_full and 'full' not in features)):
        raise ValueError('Pingap binary receipt lacks required Cargo features')
    # A cross-compiled receipt may have executed_native_probe=false. Execute the
    # final binary here on the target platform instead of trusting that flag.
    probe(binary, '--apply-protocol-version', '1')
    probe(binary, '-V', 'pingap ' + manifest['version'])
    probe(binary, '--version', f"pingap {manifest['version']} ({manifest['base_commit']}, tls={backend})")
    return {'apply_protocol_version': 1, 'binary_sha256': checksum,
            'patch_sha256': manifest['patch_sha256'], 'version': manifest['version'],
            'commit': manifest['base_commit'], 'tls_backend': backend,
            'executed_final_image_probe': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--receipt', type=Path, required=True)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--version')
    parser.add_argument('--commit')
    parser.add_argument('--require-full', action='store_true')
    parser.add_argument('--tls', choices=('openssl', 'rustls'))
    args = parser.parse_args()
    try:
        result = verify(args.binary, args.receipt, args.manifest, version=args.version,
                        commit=args.commit, require_full=args.require_full, tls=args.tls)
        print(json.dumps(result, sort_keys=True))
        return 0
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print('paired Pingap final image: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
