#!/usr/bin/env python3
"""Pingap 版本一致性门禁（batch8-followup §5）。

核对四个实际构建入口的 pingap 版本/commit 与 app-cli 单一事实源
一致：
  1. crates/app-cli/src/ 递归查找（DEFAULT_PINGAP_VERSION/COMMIT；aa07e6108 起
     位于 src/build_deploy/devtool.rs，单一常量定义）
  2. make/docker.mk（dev agent-runner 镜像构建注入）
  3. docker/build-app-runtime.py（dev app-runtime 镜像 build-arg）
  4. build-agent-docker makefiles/16-app-runtime.mk（生产镜像）

任一不一致即失败并列出全部实际值；显式允许的差异必须在此登记理由。
用法：python3 k8s/scripts/pingap_version_gate.py [--repo-root DIR]
退出码：0 一致 / 1 不一致或无法解析。
"""
import argparse
import re
import sys
from pathlib import Path

# 允许的差异登记（文件 → 理由）。当前为空——三平台同步后应保持为空。
ALLOWED_DIVERGENCE: dict[str, str] = {}

VERSION_RE = re.compile(
    r'PINGAP_VERSION\s*[?:]?=\s*"?(\d+\.\d+\.\d+)"?'
)
COMMIT_RE = re.compile(
    r'PINGAP_COMMIT\s*[?:]?=\s*"?([0-9a-f]{40})"?'
)
DEVTOOL_VERSION_RE = re.compile(r'DEFAULT_PINGAP_VERSION:\s*&str\s*=\s*"(\d+\.\d+\.\d+)"')
DEVTOOL_COMMIT_RE = re.compile(r'DEFAULT_PINGAP_COMMIT:\s*&str\s*=\s*"([0-9a-f]{40})"')


def read(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        print(f"FAIL: 无法读取 {path}: {error}")
        sys.exit(1)


def extract(text: str, patterns: list[re.Pattern]) -> tuple[str, ...] | None:
    values = []
    for pattern in patterns:
        match = pattern.search(text)
        if not match:
            return None
        values.append(match.group(1))
    return tuple(values)


def find_authority(src_root: Path) -> tuple[tuple[str, str], Path] | None:
    """递归查找 DEFAULT_PINGAP_VERSION/COMMIT 单一定义。

    aa07e6108 把 devtool.rs 从 src/ 平铺移入 src/build_deploy/；改为对
    crates/app-cli/src/ 递归查找（与 release-app-cli.yml 同法），文件再移动
    不破坏门禁。常量消失或存在冲突定义仍 fail-fast。
    """
    found: dict[tuple[str, str], Path] = {}
    for path in sorted(src_root.rglob('*.rs')):
        pair = extract(read(path), [DEVTOOL_VERSION_RE, DEVTOOL_COMMIT_RE])
        if pair is not None:
            found.setdefault(tuple(pair), path)
    if len(found) == 1:
        pair, path = next(iter(found.items()))
        return pair, path
    if len(found) > 1:
        for pair, path in found.items():
            print(f"FAIL: DEFAULT_PINGAP_VERSION/COMMIT 存在冲突定义 {pair}：{path}")
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="rcoder 仓库根（默认：脚本位置的上级上级）",
    )
    parser.add_argument(
        "--build-agent-docker",
        type=Path,
        default=Path(__file__).resolve().parents[3] / "build-agent-docker",
        help="build-agent-docker 仓库根",
    )
    args = parser.parse_args()
    root: Path = args.repo_root
    bad: Path = args.build_agent_docker

    sources: list[tuple[str, Path, list[re.Pattern]]] = [
        ("make/docker.mk（dev agent-runner 注入）", root / "make/docker.mk",
         [VERSION_RE, COMMIT_RE]),
        ("docker/build-app-runtime.py（dev app-runtime build-arg）", root / "docker/build-app-runtime.py",
         [VERSION_RE, COMMIT_RE]),
        ("build-agent-docker 16-app-runtime.mk（生产）", bad / "makefiles/16-app-runtime.mk",
         [VERSION_RE, COMMIT_RE]),
    ]

    results: dict[str, tuple[str, str] | None] = {}
    for label, path, patterns in sources:
        if not path.exists():
            print(f"FAIL: {label} 文件不存在：{path}")
            return 1
        results[label] = extract(read(path), patterns)

    authority_found = find_authority(root / "crates" / "app-cli" / "src")
    if authority_found is None:
        print("FAIL: 无法在 crates/app-cli/src 递归解析唯一的 DEFAULT_PINGAP_VERSION/COMMIT")
        return 1
    authority, authority_path = authority_found

    failures = []
    print(f"单一事实源（{authority_path.relative_to(root)}）：pingap {authority[0]} @ {authority[1][:12]}")
    for label, value in results.items():
        if label in ALLOWED_DIVERGENCE:
            print(f"  [允许差异] {label}: {value}（理由：{ALLOWED_DIVERGENCE[label]}）")
            continue
        if value != authority:
            failures.append((label, value))
            print(f"  [不一致] {label}: {value}")
        else:
            print(f"  [一致]   {label}")

    if failures:
        print(f"\nFAIL: {len(failures)} 处 pingap 版本漂移——镜像内二进制与 app-cli")
        print("      链接的 pingap-config 序列化不一致会导致 config_hash 确认恒失败")
        print("      （2026-09-19 第八批根因）。同步全部入口后重试。")
        return 1
    print("\nOK: 全部构建入口 pingap 版本一致")
    return 0


if __name__ == "__main__":
    sys.exit(main())
