#!/usr/bin/env python3
"""Read Pingap's paired source identity; parse actual executable versions exactly."""
import argparse
import json
from pathlib import Path
import re
import sys

_PRERELEASE_ID = r'(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
_SEMVER = (r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)'
           + r'(?:-' + _PRERELEASE_ID + r'(?:\.' + _PRERELEASE_ID + r')*)?'
           + r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?')


def parse_pingap_version(output):
    match = re.fullmatch(r'pingap (' + _SEMVER + r')', output.strip())
    if not match:
        raise ValueError('unexpected Pingap version output: ' + repr(output))
    return match.group(1)


def source_identity(root=None):
    root = Path(root) if root is not None else Path(__file__).resolve().parents[2]
    # 仓库源码含非 ASCII 注释；Windows runner 的默认文本编码是 cp1252，
    # 必须显式 UTF-8（否则 UnicodeDecodeError 让发布在解析阶段即失败）。
    # 版本事实源：根 [workspace.dependencies] 的 crates.io 字符串声明（0.13.x 时代的发布 bug 已修复，不再 git rev pin）。
    cargo = (root / 'Cargo.toml').read_text(encoding='utf-8')
    declarations = re.findall(r'^\s*pingap-config\s*=\s*"([^"]+)"\s*(?:#.*)?$', cargo, re.M)
    if len(declarations) != 1:
        raise ValueError('expected exactly one pingap-config version requirement')
    version = declarations[0]
    if not re.fullmatch(_SEMVER, version):
        raise ValueError('app-cli pingap-config must pin an exact semver version: ' + repr(version))
    # commit 事实源：官方资产目录（下载校验同源）。三方一致性：
    # Cargo.toml 版本 ↔ devtool 回退常量 ↔ 目录条目（tag/commit）。
    catalog_path = root / 'tools/build/pingap-assets.json'
    catalog = json.loads(catalog_path.read_text(encoding='utf-8'))
    release = catalog.get('releases', {}).get(version, {})
    commit = release.get('commit', '')
    if catalog.get('repository') != 'vicanso/pingap' or release.get('tag') != 'v' + version or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('app-cli Pingap version ' + version + ' has no trusted official asset catalog entry')
    path = root / 'crates/app-cli/src/build_deploy/devtool.rs'
    content = path.read_text(encoding='utf-8')
    def constant(name):
        matches = re.findall(r'const\s+' + name + r'\s*:\s*&str\s*=\s*"([^"]+)"\s*;', content)
        if len(matches) != 1:
            raise ValueError('expected exactly one ' + name + ' source constant')
        return matches[0]
    if constant('DEFAULT_PINGAP_VERSION') != version or constant('DEFAULT_PINGAP_COMMIT') != commit:
        raise ValueError('app-cli Pingap constants disagree with its dependency pin')
    return {'version': version, 'commit': commit}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo-root', type=Path)
    parser.add_argument('--field', choices=['version', 'commit', 'pair'])
    args = parser.parse_args()
    try:
        identity = source_identity(args.repo_root)
    except (OSError, ValueError) as error:
        print('Pingap source identity: ' + str(error), file=sys.stderr)
        return 1
    if args.field == 'pair':
        print(identity['version'] + ' ' + identity['commit'])
    elif args.field:
        print(identity[args.field])
    else:
        print(json.dumps(identity, sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
