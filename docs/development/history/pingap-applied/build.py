#!/usr/bin/env python3
"""Build the patched Pingap; write a distinct RCoder binary provenance receipt."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

from apply import apply, checked_manifest, sha256, verify_files


def checked(command, source, capture=False):
    return subprocess.run(command, cwd=source, check=True, text=True, capture_output=capture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repo-root', type=Path)
    parser.add_argument('--fetch', action='store_true')
    parser.add_argument('--tls', choices=['openssl', 'rustls'], default='openssl')
    parser.add_argument('--full', action='store_true')
    parser.add_argument('--target')
    parser.add_argument('--zig-glibc', help='Use cargo zigbuild with this glibc floor for --target')
    parser.add_argument('--already-applied', action='store_true')
    args = parser.parse_args()
    try:
        manifest, _ = checked_manifest()
        source = args.source.resolve()
        output = args.output.resolve()
        if args.fetch:
            if source.exists():
                raise ValueError('--fetch requires a new, absent source directory')
            source.mkdir(parents=True)
            checked(['git', 'init', '-q'], source)
            checked(['git', 'remote', 'add', 'origin', manifest['repository']], source)
            checked(['git', 'fetch', '--depth=1', 'origin', manifest['base_commit']], source)
            checked(['git', 'checkout', '--detach', '-q', 'FETCH_HEAD'], source)
        apply(source, root=args.repo_root, verify_only=args.already_applied)
        # JSON APIs are the RCoder contract; these informational assets also let
        # upstream embed tests run without building the management frontend.
        dist = source / 'dist'
        dist.mkdir(exist_ok=True)
        if not (dist / 'index.html').is_file():
            (dist / 'index.html').write_text('<!doctype html><title>RCoder Pingap</title><p>RCoder uses the authenticated JSON admin API.</p>\n')
        features = ['tls-rustls'] if args.tls == 'rustls' else ['openssl']
        if args.full:
            features.append('full')
        subcommand = 'zigbuild' if args.zig_glibc else 'build'
        command = ['cargo', subcommand, '--locked', '--release', '--no-default-features', '--features', ','.join(features)]
        if args.zig_glibc and not args.target:
            raise ValueError('--zig-glibc requires --target')
        if args.target:
            command += ['--target', args.target + ('.' + args.zig_glibc if args.zig_glibc else '')]
        checked(command, source)
        verify_files(source, manifest)
        target_dir = Path(os.environ.get('CARGO_TARGET_DIR', source / 'target'))
        if not target_dir.is_absolute():
            target_dir = source / target_dir
        binary = target_dir / (args.target or '') / 'release' / ('pingap.exe' if args.target and 'windows' in args.target or sys.platform == 'win32' else 'pingap')
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(binary, output)
        # Do not execute foreign cross-target executables here; CI pair gates
        # execute them on their native runners after artifact transfer.
        rustc = checked(['rustc', '-vV'], source, capture=True).stdout
        host = next(line.removeprefix('host: ') for line in rustc.splitlines() if line.startswith('host: '))
        target = args.target or host
        if target == host:
            if checked([str(output), '--apply-protocol-version'], source, capture=True).stdout.strip() != '1':
                raise ValueError('built Pingap lacks applied-status protocol 1')
            if checked([str(output), '-V'], source, capture=True).stdout.strip() != 'pingap ' + manifest['version']:
                raise ValueError('built Pingap version disagrees with paired source')
        receipt = {'protocol': 'rcoder-pingap-applied-binary-v1', 'apply_protocol_version': 1,
                   'upstream_repository': manifest['repository'], 'upstream_base_commit': manifest['base_commit'],
                   'upstream_version': manifest['version'], 'patch_sha256': manifest['patch_sha256'],
                   'binary_sha256': sha256(output), 'target': target, 'tls_backend': args.tls,
                   'cargo_features': features, 'rustc': rustc, 'executed_native_probe': target == host}
        output.with_name(output.name + '.applied.json').write_text(json.dumps(receipt, indent=2, sort_keys=True) + '\n')
        print(json.dumps(receipt, sort_keys=True))
    except (OSError, ValueError, subprocess.CalledProcessError, StopIteration) as error:
        print('paired Pingap build: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
