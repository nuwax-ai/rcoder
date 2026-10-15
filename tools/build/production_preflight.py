#!/usr/bin/env python3
"""Check a production build's actual versions against its selected CLI source.

This script deliberately does not require an adjacent RCoder checkout. Missing
vendored source is an actionable prerequisite error, never a hidden fallback.
"""
import argparse
from pathlib import Path
import re
import sys
import tomllib


def authority(source):
    src = source / 'crates/app-cli/src'
    if not src.is_dir():
        raise ValueError(f'app-cli authority missing: {src}; initialize the selected RCoder source before image preparation')
    pairs = set()
    for path in sorted(src.rglob('*.rs')):
        text = path.read_text()
        version = re.search(r'DEFAULT_PINGAP_VERSION:\s*&str\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"', text)
        commit = re.search(r'DEFAULT_PINGAP_COMMIT:\s*&str\s*=\s*"([0-9a-f]{40})"', text)
        if version and commit:
            pairs.add((version.group(1), commit.group(1)))
    if len(pairs) != 1:
        raise ValueError(f'app-cli authority is absent or conflicting: {src}')
    pair = next(iter(pairs))
    cargo = tomllib.loads((source / 'crates/app-cli/Cargo.toml').read_text())
    if cargo.get('dependencies', {}).get('pingap-config', {}).get('rev') != pair[1]:
        raise ValueError(f'pingap-config rev disagrees with CLI authority: {source}')
    return pair


def verify(args):
    if args.root is None:
        raise ValueError('runtime preflight needs --root for its versions.mk dependency source')
    configured = (args.root / 'versions.mk').read_text()
    pnpm = re.findall(r'^PNPM_MAJOR\s*\?=\s*([0-9]+)\s*$', configured, re.MULTILINE)
    if len(pnpm) != 1:
        raise ValueError('cannot parse unique PNPM_MAJOR in production versions.mk')
    if getattr(args, 'pnpm_major', None) != pnpm[0]:
        raise ValueError(f'pnpm must retain configured major {pnpm[0]}, got {getattr(args, "pnpm_major", None)}')
    rust_base_tag = getattr(args, 'rust_base_tag', None)
    if not rust_base_tag or not re.fullmatch(r'(?:stable-)?(?:trixie|bookworm)', rust_base_tag):
        raise ValueError(f'Rust base must use an explicit moving stable distribution tag, got {rust_base_tag}')
    components = getattr(args, 'component', None)
    if components is not None:
        if not components:
            raise ValueError('component preflight needs explicit components')
        verify_components(args, components)
        return None
    if args.root is None or not args.source:
        raise ValueError('full runtime preflight needs --root and selected --source')
    fields = ['pingap_version', 'pingap_commit', 'download_version', 'node_version', 'ttyd_version', 'go_version', 'deno_version']
    missing = [field.replace('_', '-') for field in fields if getattr(args, field) is None]
    if missing:
        raise ValueError('full runtime preflight missing actual arguments: ' + ', '.join(missing))
    values = []
    for name, pattern in [('PINGAP_VERSION', r'[0-9]+\.[0-9]+\.[0-9]+'), ('PINGAP_COMMIT', r'[0-9a-f]{40}')]:
        match = re.search(r'^' + name + r'\s*\?=\s*(' + pattern + r')\s*$', configured, re.MULTILINE)
        if match is None:
            raise ValueError(f'cannot parse {name} in production versions.mk')
        values.append(match.group(1))
    expected = tuple(values)
    actual = (args.pingap_version, args.pingap_commit)
    if actual != expected:
        raise ValueError(f'Pingap build arguments {actual} disagree with configured defaults {expected}')
    if args.download_version != actual[0]:
        raise ValueError(f'Pingap download version {args.download_version} disagrees with build version {actual[0]}')
    for source in args.source:
        value = authority(source)
        if value != actual:
            raise ValueError(f'Pingap {actual} disagrees with selected app-cli {value}: {source}')
    verify_components(args, ['node', 'ttyd', 'go', 'deno'])
    return actual


def verify_components(args, components):
    for component in components:
        version = getattr(args, component + '_version')
        if version is None:
            raise ValueError('component preflight missing actual ' + component + ' version')
        if component == 'node':
            if not re.fullmatch(r'22\.[0-9]+\.[0-9]+', version):
                raise ValueError(f'Node must remain on major 22, got {version}')
        elif not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
            raise ValueError(f'invalid actual {component} version: {version}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path)
    parser.add_argument('--source', type=Path, action='append')
    parser.add_argument('--component', action='append', choices=['node', 'ttyd', 'go', 'deno'], help='only check the explicitly consumed components; no Pingap source dependency')
    for flag in ['pingap-version', 'pingap-commit', 'download-version', 'node-version', 'ttyd-version', 'go-version', 'deno-version', 'pnpm-major', 'rust-base-tag']:
        parser.add_argument('--' + flag)
    args = parser.parse_args()
    try:
        pair = verify(args)
        if pair is None:
            values = '; '.join(component + ' ' + getattr(args, component + '_version') for component in args.component)
            print('Runtime component preflight OK: ' + values)
        else:
            version, commit = pair
            print(f'Runtime preflight OK: Pingap {version} @ {commit}; Node {args.node_version}; ttyd {args.ttyd_version}; Go {args.go_version}; Deno {args.deno_version}')
        return 0
    except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
        print('Runtime preflight failed: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
