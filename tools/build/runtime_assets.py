#!/usr/bin/env python3
"""Versioned runtime assets. Stable locks protect publication, never active readers.

Each reference names an immutable generation; Docker consumes that generation,
not the mutable compatibility downloads directories.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid
import zipfile


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        temporary.write_text(json.dumps(value, sort_keys=True) + '\n')
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


@contextlib.contextmanager
def locked(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'a+b') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        yield


def elf(data, arch):
    expected = {'amd64': 62, 'arm64': 183}[arch]
    if len(data) < 20 or data[:4] != b'\x7fELF' or data[4:6] != b'\x02\x01' or int.from_bytes(data[18:20], 'little') != expected:
        raise ValueError(f'invalid Linux {arch} executable')


def specs(component, version, architectures):
    for arch in architectures:
        cpu = 'x64' if arch == 'amd64' else 'arm64'
        if component == 'ttyd':
            name = 'ttyd.' + ('x86_64' if arch == 'amd64' else 'aarch64')
            url = f'https://github.com/tsl0922/ttyd/releases/download/{version}/{name}'
            yield arch, f'downloads/ttyd-{arch}', None, [url, 'https://gh-proxy.org/' + url]
        elif component == 'deno':
            name = 'x86_64' if arch == 'amd64' else 'aarch64'
            yield arch, f'cache/deno-{arch}', 'deno', [f'https://registry.npmmirror.com/-/binary/deno/v{version}/deno-{name}-unknown-linux-gnu.zip']
        elif component == 'node':
            name = f'node-v{version}-linux-{cpu}'
            yield arch, f'cache/{name}.tar.gz', name + '/bin/node', [f'https://registry.npmmirror.com/-/binary/node/v{version}/{name}.tar.gz']
        elif component == 'go':
            name = f'go{version}.linux-{arch}.tar.gz'
            yield arch, f'cache/{name}', 'go/bin/go', [f'https://golang.google.cn/dl/{name}', f'https://mirrors.aliyun.com/golang/{name}']
        elif component == 'pingap':
            name = 'pingap-linux-gnu-' + ('x86' if arch == 'amd64' else 'aarch64') + '-full'
            url = f'https://github.com/vicanso/pingap/releases/download/v{version}/{name}.tar.gz'
            yield arch, f'cache/{name.replace("pingap-", "pingap-v" + version + "-", 1)}.tar.gz', name, [url, 'https://ghproxy.net/' + url]
        else:
            raise ValueError(f'unsupported component: {component}')


def download(urls, destination):
    for url in urls:
        try:
            subprocess.run(['curl', '-fsSL', '--retry', '3', '--retry-delay', '2', '--connect-timeout', '20', '--max-time', '300', '-o', str(destination), url], check=True, timeout=950)
            return
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
            destination.unlink(missing_ok=True)
    raise RuntimeError('asset download failed: ' + urls[0])


def validate_archive(path, member, arch):
    # Read every member to detect truncation beyond the executable too. Never
    # extract an upstream pathname into the filesystem.
    with tarfile.open(path, 'r:gz') as archive:
        found = False
        for item in archive:
            if Path(item.name).is_absolute() or '..' in Path(item.name).parts:
                raise ValueError('unsafe archive path')
            if item.isfile():
                stream = archive.extractfile(item)
                if stream is None:
                    raise ValueError('archive member unreadable')
                prefix = stream.read(64)
                if item.name.removeprefix('./') == member:
                    elf(prefix, arch)
                    found = True
                while stream.read(1024 * 1024):
                    pass
        if not found:
            raise ValueError('archive executable missing: ' + member)


def verify(entry):
    entry = Path(entry)
    manifest = json.loads((entry / 'manifest.json').read_text())
    if manifest.get('protocol') != 'runtime-assets-v1' or not manifest.get('files'):
        raise ValueError('invalid runtime asset manifest')
    for relative, checksum in manifest['files'].items():
        path = entry / relative
        if Path(relative).is_absolute() or '..' in Path(relative).parts or path.is_symlink() or digest(path) != checksum:
            raise ValueError('runtime asset checksum mismatch: ' + relative)
    return manifest


def prepare(component, version, cache, architectures=('amd64', 'arm64'), downloader=download):
    cache = Path(cache).resolve() / 'v1'
    inputs = list(specs(component, version, architectures))
    identity = {'component': component, 'version': version, 'inputs': inputs, 'protocol': 'runtime-assets-v1'}
    key = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    reference = cache / 'refs' / (key + '.json')
    with locked(cache / 'locks' / (key + '.lock')):
        try:
            entry = Path(json.loads(reference.read_text())['entry'])
            if verify(entry)['identity'] == identity_json(identity):
                return entry
        except (OSError, ValueError, KeyError, TypeError):
            pass
        entries = cache / 'entries'
        entries.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=key + '-pending-', dir=entries) as temporary:
            stage = Path(temporary)
            for arch, relative, member, urls in inputs:
                target = stage / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                fetched = stage / ('download-' + arch)
                downloader(urls, fetched)
                if component == 'deno':
                    with zipfile.ZipFile(fetched) as archive:
                        if archive.testzip() is not None:
                            raise ValueError('invalid Deno zip checksum')
                        data = archive.read('deno')
                    elf(data, arch)
                    target.write_bytes(data)
                    fetched.unlink()
                else:
                    if member:
                        validate_archive(fetched, member, arch)
                    else:
                        elf(fetched.read_bytes()[:64], arch)
                    os.replace(fetched, target)
                if component in ('ttyd', 'deno'):
                    target.chmod(0o755)
            manifest = {'protocol': 'runtime-assets-v1', 'identity': identity_json(identity), 'files': {str(p.relative_to(stage)): digest(p) for p in stage.rglob('*') if p.is_file()}}
            atomic_json(stage / 'manifest.json', manifest)
            entry = entries / (key + '-' + uuid.uuid4().hex)
            os.rename(stage, entry)
            atomic_json(reference, {'entry': str(entry.resolve())})
            return entry


def identity_json(value):
    return json.loads(json.dumps(value))


def distribute(entry, contexts):
    manifest = verify(entry)
    destinations = [(Path(context).resolve(), layout)
                    for context, layout in (value if isinstance(value, tuple) else (value, 'native')
                                            for value in contexts)]
    with contextlib.ExitStack() as locks:
        for context in sorted({context for context, _ in destinations}):
            # Different versions share this stable publication lock. A failed
            # publisher must never roll back another publisher's new files.
            locks.enter_context(locked(context.parent / '.runtime-assets-locks' / (context.name + '.lock')))
        stages, staged, changed = [], [], []
        preserve_backups = False
        try:
            seen = set()
            for context, layout in destinations:
                context.mkdir(parents=True, exist_ok=True)
                stage = Path(tempfile.mkdtemp(prefix='.runtime-stage-', dir=context))
                stages.append(stage)
                for relative, checksum in manifest['files'].items():
                    target = context / ('downloads/' + Path(relative).name if layout == 'downloads' else relative)
                    if target in seen:
                        continue
                    seen.add(target)
                    if target.is_symlink() or (target.exists() and not target.is_file()):
                        raise ValueError('asset destination is not a regular file: ' + str(target))
                    target.parent.mkdir(parents=True, exist_ok=True)
                    temporary = stage / ('new-' + str(len(staged)))
                    backup = stage / ('previous-' + str(len(staged)))
                    shutil.copy2(Path(entry) / relative, temporary)
                    if digest(temporary) != checksum:
                        raise ValueError('asset changed while copying: ' + relative)
                    staged.append((temporary, target, backup))
            for temporary, target, backup in staged:
                existed = target.exists()
                if existed:
                    target.rename(backup)
                changed.append((target, backup, existed))
                os.replace(temporary, target)
        except BaseException as error:
            failures = []
            for target, backup, existed in reversed(changed):
                try:
                    target.unlink(missing_ok=True)
                    if existed:
                        backup.rename(target)
                except OSError as restore_error:
                    failures.append(str(restore_error))
            if failures:
                preserve_backups = True
                raise RuntimeError(f'asset publication failed: {error}; restoration incomplete: '
                                   + '; '.join(failures) + '; backups retained in '
                                   + ', '.join(str(stage) for stage in stages)) from error
            raise
        finally:
            if not preserve_backups:
                for stage in stages:
                    shutil.rmtree(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('component', choices=('ttyd', 'node', 'deno', 'go', 'pingap'))
    parser.add_argument('--version', required=True)
    parser.add_argument('--cache', type=Path, default=Path('.cache/runtime-assets'))
    parser.add_argument('--arch', action='append', choices=('amd64', 'arm64'))
    parser.add_argument('--context', action='append', type=Path, default=[])
    parser.add_argument('--download-context', action='append', type=Path, default=[])
    parser.add_argument('--output-ref', type=Path)
    args = parser.parse_args()
    try:
        entry = prepare(args.component, args.version, args.cache, args.arch or ('amd64', 'arm64'))
        distribute(entry, args.context + [(context, 'downloads') for context in args.download_context])
        if args.output_ref:
            atomic_json(args.output_ref, {'entry': str(entry.resolve()), 'manifest_sha256': digest(entry / 'manifest.json')})
        print(f'{args.component} {args.version}: verified and published {entry}')
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, tarfile.TarError, zipfile.BadZipFile) as error:
        print(f'{args.component}: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
