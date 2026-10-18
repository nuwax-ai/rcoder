#!/usr/bin/env python3
"""Install reviewed official Linux assets, or build an unchanged exact upstream pin.

Receipts belong to build/release verification. Runtime startup does not require
them. Source builds embed an informational page, not the upstream management UI.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def sibling(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def identity(args):
    if args.version is None or args.commit is None:
        value = sibling('pingap_identity').source_identity(args.repo_root)
        version, commit = value['version'], value['commit']
    else:
        version, commit = args.version, args.commit
    catalog = json.loads(Path(args.catalog).read_text(encoding='utf-8'))
    release = catalog.get('releases', {}).get(version, {})
    if (catalog.get('protocol') != 'pingap-release-assets-v1'
            or catalog.get('repository') != 'vicanso/pingap'
            or release.get('tag') != 'v' + version or release.get('commit') != commit):
        raise ValueError('selected identity disagrees with trusted official catalog')
    return version, commit


def checked(command, source=None, capture=False):
    return subprocess.run(command, cwd=source, check=True, text=True, capture_output=capture)


def native_probe(binary, version, commit, tls):
    if checked([str(binary), '-V'], capture=True).stdout.strip() != 'pingap ' + version:
        raise ValueError('actual Pingap version disagrees with selected source')
    expected = f'pingap {version} ({commit}, tls={tls})'
    if checked([str(binary), '--version'], capture=True).stdout.strip() != expected:
        raise ValueError('actual Pingap commit/TLS disagrees with selected source')
    # No listeners or user plugins are created by test mode. The distributed
    # app-cli-generated configuration and actual HTTP gates run separately.
    with tempfile.TemporaryDirectory(prefix='pingap-official-test-') as temporary:
        config = Path(temporary) / 'pingap.toml'
        config.write_text('[basic]\nthreads = 1\n', encoding='utf-8')
        env = {key: value for key, value in os.environ.items() if not key.startswith('PINGAP_')}
        subprocess.run([str(binary), '-c', str(config), '-t'], env=env, check=True, timeout=30)


def write_receipt(output, value):
    value.update(protocol='pingap-official-binary-v1', binary_sha256=digest(output))
    receipt = output.with_name(output.name + '.official.json')
    receipt.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n', encoding='utf-8')
    print(json.dumps(value, sort_keys=True))


def install(args, version, commit):
    # Linux-only archive machinery is loaded lazily so Windows source builds do
    # not import runtime_assets' fcntl dependency.
    assets = sibling('runtime_assets')
    catalog = json.loads(Path(args.catalog).read_text(encoding='utf-8'))
    release = assets.trusted_pingap_release(version, [args.arch], catalog)
    if release['commit'] != commit:
        raise ValueError('official release source pin mismatch')
    record = release['assets'][args.arch]
    member = record['name'].removesuffix('.tar.gz')
    with tempfile.TemporaryDirectory(prefix='pingap-official-asset-') as temporary:
        archive = args.archive
        if args.download:
            archive = Path(temporary) / record['name']
            assets.download([record['url'], 'https://ghproxy.net/' + record['url']], archive)
        if digest(archive) != record['sha256']:
            raise ValueError('trusted official asset SHA256 mismatch')
        assets.validate_archive(archive, member, args.arch)
        with tarfile.open(archive, 'r:gz') as stream:
            candidates = [item for item in stream if item.name.removeprefix('./') == member and item.isfile()]
            if len(candidates) != 1 or candidates[0].size > 512 * 1024 * 1024:
                raise ValueError('official executable missing, duplicated or oversized')
            data = stream.extractfile(candidates[0])
            if data is None:
                raise ValueError('official executable unreadable')
            args.output.parent.mkdir(parents=True, exist_ok=True)
            with open(args.output, 'wb') as destination:
                shutil.copyfileobj(data, destination)
        args.output.chmod(0o755)
    native_probe(args.output, version, commit, 'openssl')
    write_receipt(args.output, dict(origin='official_release', version=version, commit=commit,
                                   architecture=args.arch, asset_sha256=record['sha256'],
                                   tls_backend='openssl', cargo_features=['openssl', 'full'],
                                   executed_native_probe=True))


def unchanged_source(source, version, commit):
    if checked(['git', 'rev-parse', 'HEAD'], source, True).stdout.strip() != commit:
        raise ValueError('source HEAD disagrees with official exact pin')
    if checked(['git', 'status', '--porcelain', '--untracked-files=no'], source, True).stdout.strip():
        raise ValueError('official source has tracked modifications')
    cargo = tomllib.loads((source / 'Cargo.toml').read_text(encoding='utf-8'))
    package_version = cargo['package'].get('version')
    if (isinstance(package_version, dict)
            and set(package_version) == {'workspace'}
            and package_version['workspace'] is True):
        package_version = cargo.get('workspace', {}).get('package', {}).get('version')
    if package_version != version:
        raise ValueError('official source package version mismatch')


def build(args, version, commit):
    source = args.source.resolve()
    if args.fetch:
        if source.exists():
            raise ValueError('--fetch requires an absent source directory')
        source.mkdir(parents=True)
        checked(['git', 'init', '-q'], source)
        checked(['git', 'remote', 'add', 'origin', 'https://github.com/vicanso/pingap'], source)
        checked(['git', 'fetch', '--depth=1', 'origin', commit], source)
        checked(['git', 'checkout', '--detach', '-q', 'FETCH_HEAD'], source)
    unchanged_source(source, version, commit)
    dist = source / 'dist'
    dist.mkdir(exist_ok=True)
    if not (dist / 'index.html').is_file():
        (dist / 'index.html').write_text(
            '<!doctype html><title>Pingap JSON API</title><p>RCoder distributes the JSON admin API; the upstream web UI is not bundled.</p>\n',
            encoding='utf-8')
    features = ['tls-rustls'] if args.tls == 'rustls' else ['openssl']
    if args.full:
        features.append('full')
    if args.zig_glibc and not args.target:
        raise ValueError('--zig-glibc requires --target')
    command = ['cargo', 'zigbuild' if args.zig_glibc else 'build', '--locked', '--release',
               '--no-default-features', '--features', ','.join(features)]
    if args.target:
        command += ['--target', args.target + ('.' + args.zig_glibc if args.zig_glibc else '')]
    checked(command, source)
    unchanged_source(source, version, commit)
    rustc = checked(['rustc', '-vV'], source, True).stdout
    host = next(line.removeprefix('host: ') for line in rustc.splitlines() if line.startswith('host: '))
    target = args.target or host
    target_dir = Path(os.environ.get('CARGO_TARGET_DIR', source / 'target'))
    if not target_dir.is_absolute():
        target_dir = source / target_dir
    binary = target_dir / (args.target or '') / 'release' / ('pingap.exe' if 'windows' in target else 'pingap')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, args.output)
    if target == host:
        native_probe(args.output, version, commit, args.tls)
    write_receipt(args.output, dict(origin='unchanged_official_source', version=version, commit=commit,
                                   target=target, tls_backend=args.tls, cargo_features=features,
                                   rustc=rustc, executed_native_probe=target == host))


def verify(args, version, commit):
    receipt = json.loads(args.receipt.read_text(encoding='utf-8'))
    if (receipt.get('protocol') != 'pingap-official-binary-v1' or receipt.get('version') != version
            or receipt.get('commit') != commit or receipt.get('binary_sha256') != digest(args.binary)
            or receipt.get('tls_backend') not in ('openssl', 'rustls')):
        raise ValueError('official build receipt disagrees with final binary/source identity')
    if receipt.get('origin') == 'official_release':
        assets = sibling('runtime_assets')
        catalog = json.loads(Path(args.catalog).read_text(encoding='utf-8'))
        record = assets.trusted_pingap_release(version, [receipt['architecture']], catalog)['assets'][receipt['architecture']]
        if receipt.get('asset_sha256') != record['sha256']:
            raise ValueError('official receipt asset is not in trusted catalog')
        native_arch = {'x86_64': 'amd64', 'amd64': 'amd64', 'aarch64': 'arm64', 'arm64': 'arm64'}.get(platform.machine().lower())
        if receipt['architecture'] != native_arch or receipt['tls_backend'] != 'openssl':
            raise ValueError('official asset architecture/TLS differs from final native environment')
    elif receipt.get('origin') != 'unchanged_official_source':
        raise ValueError('unrecognized official binary origin')
    native_probe(args.binary, version, commit, receipt['tls_backend'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo-root', type=Path)
    parser.add_argument('--catalog', type=Path, default=Path(__file__).with_name('pingap-assets.json'))
    parser.add_argument('--version')
    parser.add_argument('--commit')
    actions = parser.add_subparsers(dest='action', required=True)
    installation = actions.add_parser('install')
    inputs = installation.add_mutually_exclusive_group(required=True)
    inputs.add_argument('--archive', type=Path)
    inputs.add_argument('--download', action='store_true')
    installation.add_argument('--arch', choices=['amd64', 'arm64'], required=True)
    installation.add_argument('--output', type=Path, required=True)
    source = actions.add_parser('build')
    source.add_argument('--source', type=Path, required=True)
    source.add_argument('--output', type=Path, required=True)
    source.add_argument('--fetch', action='store_true')
    source.add_argument('--tls', choices=['openssl', 'rustls'], default='rustls')
    source.add_argument('--full', action='store_true')
    source.add_argument('--target')
    source.add_argument('--zig-glibc')
    verification = actions.add_parser('verify')
    verification.add_argument('--binary', type=Path, required=True)
    verification.add_argument('--receipt', type=Path, required=True)
    args = parser.parse_args()
    try:
        if bool(args.version) != bool(args.commit):
            raise ValueError('--version and --commit must be supplied together')
        args.version, args.commit = identity(args)
        if args.action == 'verify':
            args.binary = args.binary.resolve()
        else:
            args.output = args.output.resolve()
        {'install': install, 'build': build, 'verify': verify}[args.action](args, args.version, args.commit)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, StopIteration, tarfile.TarError) as error:
        print('official Pingap ' + args.action + ': ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
