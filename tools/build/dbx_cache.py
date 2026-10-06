#!/usr/bin/env python3
"""DBX cache protocol v2 (keep byte-identical in both image-build repositories).

Inputs are resolved before lookup. Kernel locks have stable files; immutable
generations are never replaced or pruned while another build may consume them.
Legacy stage/stamp files are deliberately not accepted as cache identities.
"""
import argparse
from contextlib import ExitStack, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid

PROTOCOL = 2
IGNORE = '.git\n**/target\n**/node_modules\ndocs\nexamples\n.github\nagents\n'
ARCHES = ('amd64', 'arm64')


class CacheError(Exception):
    pass


def digest(value):
    return hashlib.sha256(value).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def run(arguments, **kwargs):
    result = subprocess.run([str(arg) for arg in arguments], check=False,
                            capture_output=True, text=True, **kwargs)
    if result.returncode:
        # Command lines can contain credential-bearing registry URLs. Report
        # the executable and stage, rather than echoing the complete argv.
        raise CacheError(f'{arguments[0]} failed (exit {result.returncode}): {safe_text(result.stderr)}')
    return result.stdout.strip()


def safe_text(value):
    value = re.sub(r'(https?://)[^/\s@]+@', r'\1[redacted]@', value)
    value = re.sub(r'(?i)([?&](?:token|password|secret|api_key)=)[^&\s]+', r'\1[redacted]', value)
    return value[-4000:]


@contextmanager
def lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    # Never unlink this inode: waiting processes must lock the same file.
    with path.open('a+b') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        with temporary.open('wb') as stream:
            stream.write(canonical(value) + b'\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def source_snapshot(args, work):
    source_id = digest(args.repo.encode())
    mirror = args.root / 'sources' / (source_id + '.git')
    with lock(args.root / 'locks' / ('source-' + source_id + '.lock')):
        mirror.parent.mkdir(parents=True, exist_ok=True)
        if mirror.exists():
            try:
                if run(['git', '-C', mirror, 'rev-parse', '--is-bare-repository']) != 'true':
                    raise CacheError('source cache is not a bare mirror')
            except CacheError:
                mirror.rename(mirror.with_name(mirror.name + '.invalid-' + uuid.uuid4().hex))
        if not mirror.exists():
            run(['git', 'init', '--bare', mirror])
        try:
            run(['git', '-C', mirror, 'fetch', '--depth', '1', args.fetch_repo, args.ref])
        except CacheError:
            if args.fetch_repo == args.repo:
                raise
            run(['git', '-C', mirror, 'fetch', '--depth', '1', args.repo, args.ref])
        commit = run(['git', '-C', mirror, 'rev-parse', 'FETCH_HEAD^{commit}'])
        if not re.fullmatch(r'[0-9a-f]{40,64}', commit):
            raise CacheError('source commit is not a full Git identity')
        archive = work / 'source.tar'
        run(['git', '-C', mirror, 'archive', '--format=tar', '-o', archive, commit])
    source = work / 'source'
    source.mkdir()
    with tarfile.open(archive) as tar:
        # data_filter is available on Python 3.12 and supported maintained
        # Python releases. Older versions still enforce containment below.
        for member in tar.getmembers():
            name = Path(member.name)
            if name.is_absolute() or '..' in name.parts:
                raise CacheError('source archive contains an out-of-root path')
            if member.issym() or member.islnk():
                target = (source / name.parent / member.linkname).resolve()
                if source.resolve() not in target.parents:
                    raise CacheError('source archive contains an out-of-root link')
        options = {'filter': 'data'} if hasattr(tarfile, 'data_filter') else {}
        tar.extractall(source, **options)
    archive.unlink()
    return source, commit


def toolchain_versions(args):
    versions = {'ziglang': getattr(args, 'ziglang_version', None),
                'cargo-zigbuild': getattr(args, 'cargo_zigbuild_version', None)}
    for name, version in versions.items():
        if not isinstance(version, str) or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
            raise CacheError(f'{name} needs an exact major.minor.patch version, got {version}')
    return versions


def pin_toolchain_commands(original, versions):
    # Resolve declared versions once and pin the real install commands before
    # hashing the recipe. A version recorded only in a manifest is insufficient.
    pinned, pip_count = re.subn(
        r'(\bpip3?\s+install\b[^;\n]*?\s+)ziglang(?:==[A-Za-z0-9_.+-]+)?(?=\s|[;&\\]|$)',
        lambda match: match.group(1) + 'ziglang==' + versions['ziglang'], original)
    pinned, cargo_count = re.subn(
        r'\bcargo\s+install\s+cargo-zigbuild\b',
        'cargo install --locked --version ' + versions['cargo-zigbuild'] + ' cargo-zigbuild', pinned)
    if pip_count != 1 or cargo_count != 1:
        raise CacheError('DBX Dockerfile must contain one recognized ziglang and cargo-zigbuild install command; '
                         f'found ziglang={pip_count}, cargo-zigbuild={cargo_count}')
    return pinned


def resolve_inputs(args, source, commit):
    dockerfile = source / 'deploy/Dockerfile'
    versions = toolchain_versions(args)
    original = pin_toolchain_commands(dockerfile.read_text(), versions)
    references = {}
    stages = set()
    for line in original.splitlines():
        match = re.match(r'(?i)^FROM\s+(?:--platform=\S+\s+)?(\S+)(?:\s+AS\s+(\S+))?\s*$', line)
        if not match:
            continue
        image, stage = match.groups()
        if image.lower() not in stages and image != 'scratch':
            if '$' in image:
                raise CacheError('DBX Dockerfile FROM variables need explicit resolution')
            if '@sha256:' in image:
                image_digest = image.split('@', 1)[1]
            else:
                image_digest = image_identity(image)
            if not isinstance(image_digest, str) or not re.fullmatch(r'sha256:[0-9a-f]{64}', image_digest):
                raise CacheError('base image inspection returned an invalid digest')
            references[image] = image_digest
        if stage:
            stages.add(stage.lower())
    if not references:
        raise CacheError('DBX Dockerfile has no resolved base images')
    pinned = original
    for image, image_digest in references.items():
        pinned = re.sub(r'(?im)^(FROM\s+(?:--platform=\S+\s+)?)' + re.escape(image) + r'(?=\s|$)',
                        lambda match: match.group(1) + image.split('@')[0] + '@' + image_digest, pinned)
    # Follow the source project's pinned package manager, not npm's moving
    # latest. This keeps Node 22 builds and the declared frontend input aligned.
    package = json.loads((source / 'package.json').read_text())
    manager = package.get('packageManager', '')
    if re.fullmatch(r'pnpm@[0-9]+\.[0-9]+\.[0-9]+(?:\+sha[0-9]+\.[0-9a-f]+)?', manager):
        pinned = pinned.replace('npm i -g pnpm\n', 'npm i -g ' + manager.split('+')[0] + '\n')
    (source / '.dockerignore').write_text(IGNORE)
    dockerfile.write_text(pinned)
    inspection = run(['docker', 'buildx', 'inspect', args.builder])
    builder_details = [line.strip() for line in inspection.splitlines()
                       if re.match(r'\s*(?:Name|Driver|Endpoint|BuildKit version|Platforms):', line)]
    if not builder_details:
        raise CacheError('builder inspection did not return an effective build configuration')
    inputs = {
        'protocol': PROTOCOL,
        'recipe_sha256': digest(Path(__file__).read_bytes()),
        'source_commit': commit,
        'source_repo': safe_text(args.repo),
        'source_repo_sha256': digest(args.repo.encode()),
        'architectures': list(ARCHES),
        'dockerfile_sha256': digest(pinned.encode()),
        'dockerignore_sha256': digest(IGNORE.encode()),
        'base_images': references,
        'build_args': {'PIP_INDEX_URL': safe_text(args.pip_index)},
        'build_args_sha256': digest(canonical({'PIP_INDEX_URL': args.pip_index})),
        'package_manager': manager,
        'toolchains': versions,
        'build_platform': run(['docker', 'version', '--format', '{{.Server.Os}}/{{.Server.Arch}}']),
        'builder': args.builder,
        'builder_configuration': builder_details,
    }
    return inputs


def image_identity(image):
    raw = run(['docker', 'buildx', 'imagetools', 'inspect', image,
               '--format', '{{json .Manifest.Digest}}'])
    try:
        image_digest = json.loads(raw)
    except json.JSONDecodeError as error:
        raise CacheError('image inspection did not return a digest') from error
    if not isinstance(image_digest, str) or not re.fullmatch(r'sha256:[0-9a-f]{64}', image_digest):
        raise CacheError('image inspection returned an invalid digest')
    return image_digest


def inventory(directory):
    files = {}
    for item in sorted(directory.rglob('*')):
        if item.is_symlink():
            raise CacheError('DBX assets must not contain symlinks')
        if item.is_file() and item != directory / 'manifest.json':
            sha = hashlib.sha256()
            with item.open('rb') as stream:
                for block in iter(lambda: stream.read(1024 * 1024), b''):
                    sha.update(block)
            files[item.relative_to(directory).as_posix()] = {
                'sha256': sha.hexdigest(), 'size': item.stat().st_size,
                'mode': item.stat().st_mode & 0o777,
            }
    return files


def validate_assets(directory, require_fork=True):
    for arch, machine in [('amd64', 62), ('arm64', 183)]:
        binary = directory / ('dbx-web-' + arch)
        with binary.open('rb') as stream:
            header = stream.read(64)
            if len(header) < 64 or header[:4] != b'\x7fELF' or header[4:6] != b'\x02\x01':
                raise CacheError(f'dbx-web-{arch} is not a Linux ELF64 binary')
            if int.from_bytes(header[18:20], 'little') != machine:
                raise CacheError(f'dbx-web-{arch} has the wrong architecture')
            if require_fork:
                found, previous = b'ensure-local-pg' in header, header[-32:]
                for block in iter(lambda: stream.read(1024 * 1024), b''):
                    if b'ensure-local-pg' in previous + block:
                        found = True
                        break
                    previous = block[-32:]
                if b'ensure-local-pg' in header:
                    found = True
                if not found:
                    raise CacheError(f'dbx-web-{arch} is missing the fork marker')
        if not os.access(binary, os.X_OK):
            raise CacheError(f'dbx-web-{arch} is not executable')
    static = directory / 'dbx-static'
    if not static.is_dir() or not (static / 'index.html').is_file() or not (static / 'index.html').stat().st_size:
        raise CacheError('DBX static frontend is missing index.html')
    return inventory(directory)


def validate_entry(entry, inputs=None):
    manifest = json.loads((entry / 'manifest.json').read_text())
    if manifest.get('protocol') != PROTOCOL or (inputs is not None and manifest.get('inputs') != inputs):
        raise CacheError('DBX cache input identity does not match')
    if manifest.get('key') != digest(canonical(manifest['inputs'])):
        raise CacheError('DBX cache key does not match its inputs')
    files = validate_assets(entry, manifest['inputs'].get('kind') != 'official-image')
    if manifest.get('files') != files:
        raise CacheError('DBX cache asset checksums do not match')
    return manifest


def extract_image(image, arch, artifacts, static):
    container = run(['docker', 'create', '--platform', 'linux/' + arch, image])
    if not re.fullmatch(r'[0-9a-f]{64}', container):
        raise CacheError('docker create did not return one container identity')
    failure = None
    try:
        binary = artifacts / ('dbx-web-' + arch)
        run(['docker', 'cp', container + ':/usr/local/bin/dbx-web', binary])
        binary.chmod(0o755)
        if static:
            run(['docker', 'cp', container + ':/app/static', artifacts / 'dbx-static'])
    except (OSError, CacheError) as error:
        failure = error
    try:
        run(['docker', 'rm', '-f', container])
    except CacheError as error:
        if failure is not None:
            raise CacheError(f'{failure}; container cleanup also failed: {error}') from error
        raise
    if failure is not None:
        raise failure


def build_images(args, source, work, artifacts, key):
    images = {}
    for arch in ARCHES:
        tag = 'dbx-fork-v2:' + key[:16] + '-' + arch + '-' + uuid.uuid4().hex
        iid = work / ('image-' + arch + '.iid')
        run(['docker', 'buildx', 'build', '--builder', args.builder,
             '--platform', 'linux/' + arch, '--file', 'deploy/Dockerfile',
             '--tag', tag, '--iidfile', iid,
             '--build-arg', 'PIP_INDEX_URL=' + args.pip_index, '--load', '.'], cwd=source)
        image = iid.read_text().strip()
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', image):
            raise CacheError('BuildKit did not return an exact image identity')
        images[arch] = image
        # The unique tag is diagnostic only; extraction is bound to --iidfile.
        extract_image(image, arch, artifacts, arch == ARCHES[0])
    return images


def publish_contexts(entry, contexts, root):
    manifest = validate_entry(entry)
    contexts = sorted({context.resolve() for context in contexts})
    with ExitStack() as locks, ExitStack() as temporary:
        prepared = []
        for context in contexts:
            # Lock lives beside the downloads directory, so replacing staging
            # content cannot create a different lock inode.
            locks.enter_context(lock(context.parent / '.dbx-locks' / (context.name + '.lock')))
            context.mkdir(parents=True, exist_ok=True)
            stage = Path(temporary.enter_context(tempfile.TemporaryDirectory(prefix='.dbx-stage-', dir=context)))
            for arch in ARCHES:
                shutil.copy2(entry / ('dbx-web-' + arch), stage)
            shutil.copytree(entry / 'dbx-static', stage / 'dbx-static')
            if validate_assets(stage, manifest['inputs'].get('kind') != 'official-image') != manifest['files']:
                raise CacheError('copied DBX assets failed checksum verification')
            atomic_json(stage / '.dbx-manifest.json', {**manifest, 'entry': str(entry)})
            prepared.append((context, stage))
        changes = []
        try:
            for context, stage in prepared:
                for name in ['dbx-web-amd64', 'dbx-web-arm64', 'dbx-static', '.dbx-manifest.json']:
                    destination = context / name
                    backup = stage / (name + '.previous')
                    existed = destination.exists() or destination.is_symlink()
                    if existed:
                        destination.rename(backup)
                    changes.append((destination, backup, existed))
                    (stage / name).rename(destination)
        except BaseException:
            for destination, backup, existed in reversed(changes):
                if destination.is_dir() and not destination.is_symlink():
                    shutil.rmtree(destination)
                else:
                    destination.unlink(missing_ok=True)
                if existed:
                    backup.rename(destination)
            raise
    return manifest


def find_entry(root, key, inputs):
    try:
        reference = json.loads((root / 'refs' / (key + '.json')).read_text())
        entry = root / 'entries' / reference['generation']
        if entry.parent.resolve() != (root / 'entries').resolve():
            raise CacheError('DBX cache reference is out of bounds')
        if reference.get('manifest_sha256') != digest((entry / 'manifest.json').read_bytes()):
            raise CacheError('DBX cache manifest checksum does not match')
        validate_entry(entry, inputs)
        return entry
    except (OSError, ValueError, KeyError, CacheError):
        return None


def build(args):
    args.root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='dbx-v2-', dir=args.root) as temporary:
        work = Path(temporary)
        source, commit = source_snapshot(args, work)
        inputs = resolve_inputs(args, source, commit)
        key = digest(canonical(inputs))
        with lock(args.root / 'locks' / ('build-' + key + '.lock')):
            entry = None if args.force else find_entry(args.root, key, inputs)
            if entry is None:
                artifacts = work / 'artifacts'
                artifacts.mkdir()
                images = build_images(args, source, work, artifacts, key)
                files = validate_assets(artifacts)
                manifest = {'protocol': PROTOCOL, 'key': key, 'inputs': inputs,
                            'images': images, 'files': files}
                atomic_json(artifacts / 'manifest.json', manifest)
                generation = key + '-' + uuid.uuid4().hex
                entry = args.root / 'entries' / generation
                entry.parent.mkdir(parents=True, exist_ok=True)
                artifacts.rename(entry)
                atomic_json(args.root / 'refs' / (key + '.json'), {
                    'generation': generation,
                    'manifest_sha256': digest((entry / 'manifest.json').read_bytes()),
                })
            publish_contexts(entry, args.context, args.root)
            # Only complete distribution publishes a success reference.
            atomic_json(args.root / 'last-success.json', {'entry': str(entry)})
            if args.output_ref:
                atomic_json(args.output_ref, entry_reference(entry, key))
            print('✅ DBX v2 ready: ' + str(entry))


def official(args):
    # Resolve a mutable tag once, then pull/create that exact multiarch digest.
    resolved = image_identity(args.image)
    image = args.image.split('@')[0] + '@' + resolved
    inputs = {'protocol': PROTOCOL, 'kind': 'official-image',
              'recipe_sha256': digest(Path(__file__).read_bytes()),
              'image': image, 'architectures': list(ARCHES)}
    key = digest(canonical(inputs))
    args.root.mkdir(parents=True, exist_ok=True)
    with lock(args.root / 'locks' / ('build-' + key + '.lock')):
        entry = None if args.force else find_entry(args.root, key, inputs)
        if entry is None:
            with tempfile.TemporaryDirectory(prefix='dbx-official-', dir=args.root) as temporary:
                artifacts = Path(temporary) / 'artifacts'
                artifacts.mkdir()
                for arch in ARCHES:
                    run(['docker', 'pull', '--platform', 'linux/' + arch, image])
                    extract_image(image, arch, artifacts, arch == ARCHES[0])
                files = validate_assets(artifacts, require_fork=False)
                atomic_json(artifacts / 'manifest.json', {
                    'protocol': PROTOCOL, 'key': key, 'inputs': inputs, 'files': files,
                })
                generation = key + '-' + uuid.uuid4().hex
                entry = args.root / 'entries' / generation
                entry.parent.mkdir(parents=True, exist_ok=True)
                artifacts.rename(entry)
                atomic_json(args.root / 'refs' / (key + '.json'), {
                    'generation': generation,
                    'manifest_sha256': digest((entry / 'manifest.json').read_bytes()),
                })
        publish_contexts(entry, args.context, args.root)
        atomic_json(args.root / 'last-success.json', {'entry': str(entry)})
        if args.output_ref:
            atomic_json(args.output_ref, entry_reference(entry, key))
        print('✅ DBX official v2 ready: ' + str(entry))


def distribute(args):
    if args.stage:
        entry = args.stage.resolve()
    else:
        entry = Path(json.loads((args.root / 'last-success.json').read_text())['entry'])
    publish_contexts(entry, args.context, args.root)
    if args.output_ref:
        manifest = validate_entry(entry)
        atomic_json(args.output_ref, entry_reference(entry, manifest['key']))
    print('✅ DBX v2 distributed: ' + str(entry))


def entry_reference(entry, key):
    return {'entry': str(entry), 'key': key, 'protocol': PROTOCOL,
            'manifest_sha256': digest((entry / 'manifest.json').read_bytes())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['build', 'official', 'update', 'distribute', 'clean-source'])
    parser.add_argument('--root', type=Path, required=True, help='persistent DBX root; protocol data is in v2/')
    parser.add_argument('--repo', default='https://github.com/nuwax-ai/dbx.git')
    parser.add_argument('--fetch-repo')
    parser.add_argument('--ref', default='test')
    parser.add_argument('--builder', default='default')
    parser.add_argument('--pip-index', default='https://mirrors.aliyun.com/pypi/simple')
    parser.add_argument('--ziglang-version')
    parser.add_argument('--cargo-zigbuild-version')
    parser.add_argument('--context', action='append', type=Path, default=[])
    parser.add_argument('--output-ref', type=Path)
    parser.add_argument('--stage', type=Path, help='explicit v2 immutable entry, never an unverified legacy stage')
    parser.add_argument('--force', action='store_true')
    parser.add_argument('--image', default='docker.cnb.cool/dbxio.com/dbx:latest')
    args = parser.parse_args()
    args.root = args.root.expanduser().resolve() / 'v2'
    args.fetch_repo = args.fetch_repo or args.repo
    try:
        if args.repo.startswith('-') or args.fetch_repo.startswith('-') or args.ref.startswith('-'):
            raise CacheError('source repository and ref must not be Git options')
        if args.command == 'build':
            toolchain_versions(args)
            build(args)
        elif args.command == 'official':
            official(args)
        elif args.command == 'distribute':
            distribute(args)
        elif args.command == 'update':
            args.root.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryDirectory(prefix='dbx-source-', dir=args.root) as temporary:
                _, commit = source_snapshot(args, Path(temporary))
                print('DBX source commit: ' + commit)
        else:
            source_id = digest(args.repo.encode())
            with lock(args.root / 'locks' / ('source-' + source_id + '.lock')):
                mirror = args.root / 'sources' / (source_id + '.git')
                if mirror.exists():
                    shutil.rmtree(mirror)
            print('DBX source mirror cleared; immutable assets and locks retained')
    except (CacheError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print('DBX v2 failed: ' + safe_text(str(error)), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
