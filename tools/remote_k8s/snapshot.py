"""Allowlisted snapshots: mirrored extras can never become build inputs."""
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
from common import EXCLUDES, ROOT, digest, run


def excluded(path):
    parts = Path(path).parts
    for pattern in EXCLUDES:
        anchored = pattern.startswith('/')
        pattern = pattern.lstrip('/')
        if fnmatch.fnmatch(path, pattern) or path.startswith(pattern.rstrip('/') + '/'):
            return True
        if not anchored and '/' not in pattern and any(fnmatch.fnmatch(p, pattern) for p in parts):
            return True
    return False


def entry(path):
    mode = path.lstat().st_mode
    if stat.S_ISLNK(mode):
        target = os.readlink(path)
        if os.path.isabs(target) or not path.resolve().is_relative_to(ROOT.resolve()):
            raise ValueError(f'Link escapes repository: {path}')
        return {'kind': 'link', 'target': target}
    if not stat.S_ISREG(mode):
        raise ValueError(f'Unsupported source entry: {path}')
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return {'kind': 'file', 'sha256': h.hexdigest(), 'executable': bool(mode & 0o111)}


def manifest():
    paths = run(['git', '-C', ROOT, 'ls-files', '-z', '--cached', '--others', '--exclude-standard']).split('\0')
    ignored = subprocess.run(['git', '-C', str(ROOT), 'check-ignore', '--no-index', '-z', '--stdin'],
                             input='\0'.join(p for p in paths if p) + '\0', text=True, capture_output=True)
    if ignored.returncode not in (0, 1):
        raise RuntimeError('Cannot evaluate source ignore rules')
    ignored_paths = set(ignored.stdout.split('\0'))
    result = {}
    for name in sorted(set(paths)):
        if not name or name in ignored_paths or excluded(name):
            continue
        path = ROOT / name
        if path.exists() or path.is_symlink():
            result[name] = entry(path)
    if 'Cargo.lock' not in result or 'Cargo.toml' not in result:
        raise ValueError('Source manifest has no Cargo workspace')
    return result


# Sent via SSH stdin, not interpolated into a shell command. Never hard-link live files.
REMOTE_SNAPSHOT = r'''
import hashlib,json,os,pathlib,shutil,stat,sys
root=pathlib.Path(sys.argv[1]); ident=sys.argv[2]
manifest=json.load(sys.stdin)
source=root/'live'; dest=root/'snapshots'/ident
if dest.exists(): raise RuntimeError('Snapshot already exists')
dest.mkdir(parents=True)
try:
 for name, expected in manifest.items():
  relative=pathlib.PurePosixPath(name)
  if relative.is_absolute() or '..' in relative.parts: raise RuntimeError('Invalid path')
  src=source/name; dst=dest/name
  if not src.parent.resolve().is_relative_to(source.resolve()): raise RuntimeError('Symlink ancestor')
  dst.parent.mkdir(parents=True,exist_ok=True)
  if expected['kind']=='link':
   if not src.is_symlink() or os.readlink(src)!=expected['target']: raise RuntimeError('Link mismatch: '+name)
   dst.symlink_to(expected['target'])
  else:
   if src.is_symlink() or not src.is_file(): raise RuntimeError('Type mismatch: '+name)
   shutil.copy2(src,dst)
   actual=hashlib.sha256(dst.read_bytes()).hexdigest()
   if actual!=expected['sha256'] or bool(dst.stat().st_mode&0o111)!=expected['executable']: raise RuntimeError('Content mismatch: '+name)
 for p in dest.rglob('*'):
  if p.is_symlink() and not p.resolve().is_relative_to(dest.resolve()): raise RuntimeError('Snapshot link escapes')
 print(json.dumps({'path':str(dest),'files':len(manifest)}))
except BaseException:
 shutil.rmtree(dest)
 raise
'''


def create(config, build_id, expected=None):
    """冻结远端构建快照。`expected` 给出 verify 轮预先冻结的清单——构建输入
    必须与该清单逐字一致（同轮输入硬保证），不一致立即失败而非各自漂移。"""
    for attempt in range(3):
        before = manifest()
        if expected is not None and before != expected:
            raise RuntimeError('Live source changed between verify freeze and build; rerun verify')
        config.mut('sync', 'flush', config.session)
        sessions = json.loads(config.mut('sync', 'list', config.session, '--template', '{{json .}}'))
        if len(sessions) != 1 or sessions[0].get('conflicts') or sessions[0].get('lastError') or any(
                not sessions[0].get(side, {}).get('connected') or
                sessions[0].get(side, {}).get('scanProblems') or
                sessions[0].get(side, {}).get('transitionProblems') for side in ['alpha', 'beta']):
            raise RuntimeError('Synchronization has conflicts or errors')
        name = build_id + '-' + str(attempt)
        try:
            result = json.loads(config.ssh(['python3', '-c', REMOTE_SNAPSHOT, config.remote, name], json.dumps(before)))
        except RuntimeError:
            if before != manifest() and attempt < 2:
                continue
            raise
        if before == manifest():
            return {**result, 'source_sha256': digest(before), 'manifest': before}
    raise RuntimeError('Source kept changing while freezing snapshot; retry after edits settle')
