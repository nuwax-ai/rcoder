"""Conservative build reuse (R2).

同一有效输入的完整成功产物可复用：cache key 覆盖 schema、**完整源码清单
摘要**（保守阶段：任何文件变化都 miss，文档暂不免编译）、三个基础镜像
digest、Rust 构建镜像、目标平台、影响产物的 build args 与镜像目标
（Dockerfile 与构建脚本在源码清单内随摘要一起变化）。

规则：
- 只接纳 `status='built'` 且三目标齐全的完整 receipt；失败/半成品永不命中；
- 命中后必须 registry 产物存在且 digest 匹配（imagetools inspect），不存在
  → 正常重建；权限/网络错误 → 明确报告（不得当作命中，也不得当作成功）；
- 复用记录 reused_from/cache_key/来源摘要，不伪造新构建记录；
- 未知依赖变化按受影响处理（保守 miss）。Cargo mtime touch 保护保留。
"""
import json
import re

from common import atomic_json, digest

CACHE_SCHEMA = 1
TARGETS = ['rcoder', 'computer', 'runtime']


def cache_key(source_sha256, bases, config):
    """整源保守 key：任一输入维度变化即 miss。三目标共用同一 key 空间，
    key 内含目标名（计算机/运行时镜像不可因只改 rcoder 而互相误复用——
    保守阶段三目标同 key 命中或同 miss，目标级细分属 C 批）。"""
    return digest({
        'schema': CACHE_SCHEMA,
        'source_sha256': source_sha256,
        'bases': bases,
        'rust_image': config.get('RUST_IMAGE', 'rust:1.95-trixie'),
        'cargo_jobs': config.get('JOBS', '4'),
        'apt_mirror': config.get('APT_MIRROR', 'http://deb.debian.org'),
        'cargo_mirror': config.get('CARGO_MIRROR', ''),
        'platform': 'linux/amd64',
    })


def load(config, key):
    """读取完整成功 receipt；结构不符/不完整 → None（永不命中半成品）。"""
    path = config.state / 'cache' / 'builds' / (key + '.json')
    if not path.exists():
        return None
    try:
        row = json.loads(path.read_text())
    except ValueError:
        return None
    if row.get('key') != key or row.get('status') != 'built':
        return None
    if set(row.get('images', {})) != set(TARGETS):
        return None
    if any(not re.fullmatch(r'.+@sha256:[a-f0-9]{64}', value) for value in row['images'].values()):
        return None
    return row


def store(config, key, receipt):
    atomic_json(config.state / 'cache' / 'builds' / (key + '.json'), {
        'schema': CACHE_SCHEMA, 'key': key, 'status': 'built',
        'images': dict(receipt['images']), 'bases': dict(receipt['bases']),
        'build_id': receipt['build_id'], 'source_sha256': receipt['source_sha256'],
        'finished_at': receipt.get('finished_at'),
    })


def artifact_matches(config, reference):
    """registry 产物校验：存在且 digest 与引用一致。

    返回 (ok, error_class)：error_class 仅在 ok=False 时有意义——
    artifact-missing 触发正常重建；registry-error 是权限/网络故障，
    调用方必须明确报告，不得静默当 miss 或成功。
    """
    try:
        info = config.ssh(['docker', 'buildx', 'imagetools', 'inspect', reference], timeout=180)
    except RuntimeError as error:
        message = str(error).lower()
        if 'manifest unknown' in message or 'not found' in message or 'exited 1' in message and 'unknown' in message:
            return False, 'artifact-missing'
        return False, 'registry-error'
    match = re.search(r'^Digest:\s*(sha256:[a-f0-9]{64})', info, re.M)
    if not match:
        return False, 'registry-error'
    return match.group(1) == reference.split('@')[-1], 'digest-mismatch'
