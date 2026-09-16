"""Frozen local test snapshots (R1).

验收测试消费**封存的本地快照**，不再读取活动工作目录：
- 冻结：沿用与构建快照相同的 manifest 语义（含未提交/未跟踪文件，凭据与
  输出目录排除），复制到 `.remote-k8s/<环境>/test-snapshots/<id>/source/`，
  保留执行位；符号链接仅允许指向快照内部（拒绝绝对/逃逸链接）；
- 封存校验：复制内容与清单逐项比对（sha/执行位/链接目标），冻结期间输入
  端变化则清理重试（有限次数）；
- 运行前/后校验：快照目录相对清单的 新增/删除/内容/链接 任何变化都会失败
  （捕获篡改与逃逸）；报告与缓存目录在状态树下，永不进入输入指纹；
- 活动工作目录在本轮期间的变化**不**导致失败，仅作为提示记录。

不使用 `git clone --shared`（引用活动对象库，源仓库 GC 后可能损坏）；快照
目录无 .git，执行器经显式 E2E_SOURCE_ROOT/E2E_INPUT_MANIFEST 运行。
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import uuid

from common import ROOT, atomic_json, digest
from snapshot import manifest as source_manifest

SNAPSHOT_SCHEMA = 1


def _safe_relative(name):
    path = Path(name)
    if path.is_absolute() or '..' in path.parts:
        raise ValueError('Invalid snapshot path: ' + name)
    return path


def _copy_entries(frozen_manifest, destination):
    destination.mkdir(parents=True)
    for name, expected in frozen_manifest.items():
        src = ROOT / _safe_relative(name)
        dst = destination / name
        dst.parent.mkdir(parents=True, exist_ok=True)
        if expected['kind'] == 'link':
            dst.symlink_to(expected['target'])
            continue
        shutil.copy2(src, dst)
        if expected['executable']:
            dst.chmod(dst.stat().st_mode | 0o111)


def _verify_entries(frozen_manifest, root):
    """快照目录 vs 冻结清单：新增/删除/内容/执行位/链接目标/逃逸链接全比对。

    目录不是可执行输入：仅当它不是任何清单条目的祖先时才算"新增"（空目录
    不构成输入漂移；新增文件必然被其自身条目比对捕获）。
    """
    problems = []
    allowed_directories = set()
    for name in frozen_manifest:
        parent = Path(name).parent
        while parent != Path('.'):
            allowed_directories.add(parent.as_posix())
            parent = parent.parent
    for path in root.rglob('*'):
        relative = path.relative_to(root).as_posix()
        if relative in frozen_manifest:
            continue
        if path.is_dir() and not path.is_symlink() and relative in allowed_directories:
            continue
        problems.append('unexpected entry in snapshot: ' + relative)
    for name, expected in frozen_manifest.items():
        path = root / _safe_relative(name)
        if not path.exists() and not path.is_symlink():
            problems.append('missing snapshot entry: ' + name)
            continue
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            target = os.readlink(path)
            if expected['kind'] != 'link' or target != expected['target']:
                problems.append('link mismatch in snapshot: ' + name)
            if path.is_symlink() and not path.resolve().is_relative_to(root.resolve()):
                problems.append('snapshot link escapes: ' + name)
            continue
        if not stat.S_ISREG(mode):
            problems.append('unsupported snapshot entry: ' + name)
            continue
        h = hashlib.sha256()
        with path.open('rb') as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                h.update(block)
        if expected['kind'] != 'file' or h.hexdigest() != expected['sha256'] \
                or bool(mode & 0o111) != expected['executable']:
            problems.append('content mismatch in snapshot: ' + name)
    return problems


def freeze(config, snapshot_id=None):
    """冻结当前测试输入。返回快照记录（含清单与身份摘要）。

    冻结期间活动目录变化 → 清理本轮未完成目录并重试（至多 3 次）。
    """
    for attempt in range(3):
        frozen_manifest = source_manifest()
        record = _freeze_with(config, frozen_manifest)
        if frozen_manifest == source_manifest():
            return record
        shutil.rmtree(Path(record['path']))  # 冻结期间输入变化：清理重试
    raise RuntimeError('Source kept changing while freezing the test snapshot; retry after edits settle')


def freeze_from_manifest(config, frozen_manifest):
    """verify 同轮冻结：按给定清单从活动目录复制（活动目录必须仍与该清单
    一致——build 侧的 snapshot.create(expected=...) 会再校验一次）。"""
    if frozen_manifest != source_manifest():
        raise RuntimeError('Live source changed before the same-round freeze completed; rerun verify')
    return _freeze_with(config, frozen_manifest)


def _freeze_with(config, frozen_manifest):
    identity = digest(frozen_manifest)
    snapshot_id = datetime_stamp() + '-' + uuid.uuid4().hex[:8]
    destination = config.state / 'test-snapshots' / snapshot_id
    if destination.exists():
        raise RuntimeError('Test snapshot already exists: ' + snapshot_id)
    record = {'schema': SNAPSHOT_SCHEMA, 'snapshot_id': snapshot_id,
              'environment': config.id, 'status': 'freezing',
              'source_sha256': identity, 'files': len(frozen_manifest)}
    try:
        freeze_started = time_now()
        _copy_entries(frozen_manifest, destination / 'source')
        problems = _verify_entries(frozen_manifest, destination / 'source')
        if problems:
            raise RuntimeError('Snapshot seal failed: ' + '; '.join(problems[:5]))
        atomic_json(destination / 'inputs.json', frozen_manifest)
        record['status'] = 'sealed'
        record['path'] = str(destination)
        record['freeze_ms'] = int((time_now() - freeze_started) * 1000)
        atomic_json(destination / 'snapshot.json', record)
        return record
    except BaseException:
        shutil.rmtree(destination, ignore_errors=True)
        raise


def time_now():
    import time
    return time.monotonic()


def verify(record):
    """运行前/后校验快照未被修改（新增/删除/内容/链接/逃逸）。"""
    destination = Path(record['path'])
    frozen_manifest = json.loads((destination / 'inputs.json').read_text())
    problems = _verify_entries(frozen_manifest, destination / 'source')
    extra = [p.name for p in destination.iterdir()
             if p.name not in ('source', 'inputs.json', 'snapshot.json')]
    if extra:
        problems.append('unexpected snapshot metadata: ' + ', '.join(sorted(extra)))
    if digest(frozen_manifest) != record['source_sha256']:
        problems.append('snapshot manifest identity mismatch')
    if problems:
        raise RuntimeError('Frozen test snapshot was modified: ' + '; '.join(problems[:5]))
    return True


def datetime_stamp():
    import datetime
    return datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
