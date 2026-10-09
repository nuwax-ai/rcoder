#!/usr/bin/env python3
"""核验 Pingap 源码契约与实际构建参数。

默认版本由 app-cli 源码身份派生。门禁执行无副作用的 Make/Python 身份
打印接口，核验真实默认值及环境覆盖；仅显式 --cross-repo 时读取构建仓。
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from tools.build.pingap_identity import source_identity


def environment(defaults=False):
    env = os.environ.copy()
    # 子 Make 不应继承调用者的目标、-n/-j 或命令行变量。实际构建参数在下方
    # 独立核验，默认值检查也不能被正确的显式覆盖掩盖。
    for name in ('MAKEFLAGS', 'MFLAGS', 'MAKELEVEL', 'MAKEOVERRIDES', 'GNUMAKEFLAGS', 'MAKEFILES'):
        env.pop(name, None)
    if defaults:
        for name in ('PINGAP_VERSION', 'PINGAP_COMMIT', 'PINGAP_DL_VERSION'):
            env.pop(name, None)
    return env


def read_pair(command, root, env):
    result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise ValueError('身份打印失败: ' + (result.stderr.strip() or result.stdout.strip()))
    values = result.stdout.strip().split()
    if len(values) != 2 or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', values[0]) or not re.fullmatch(r'[0-9a-f]{40}', values[1]):
        raise ValueError('身份打印结果格式错误: ' + repr(result.stdout))
    return tuple(values)


def consumer_pairs(root, env):
    return {
        'make/docker.mk（dev agent-runner 注入）': read_pair(
            ['make', '--no-print-directory', '-s', '-f', 'make/docker.mk', 'print-pingap-build-identity'], root, env),
        'docker/build-app-runtime.py（dev app-runtime build-arg）': read_pair(
            [sys.executable, str(root / 'docker/build-app-runtime.py'), '--print-pingap-identity'], root, env),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo-root', type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument('--build-agent-docker', type=Path)
    parser.add_argument('--cross-repo', action='store_true', help='显式核对构建仓版本和启动脚本')
    parser.add_argument('--pingap-version')
    parser.add_argument('--pingap-commit')
    parser.add_argument('--download-version')
    parser.add_argument('--node-version')
    args = parser.parse_args()
    root = args.repo_root.resolve()
    bad = (args.build_agent_docker or root.parent / 'build-agent-docker').resolve()
    if args.node_version and not re.fullmatch(r'22\.[0-9]+\.[0-9]+', args.node_version):
        print('FAIL: Node 运行时必须保持 22，当前输入 ' + args.node_version)
        return 1
    try:
        identity = source_identity(root)
    except (OSError, ValueError) as error:
        print('FAIL: app-cli 实际 pingap-config rev 或运行时身份不一致: ' + str(error))
        return 1
    authority = (identity['version'], identity['commit'])
    try:
        defaults = consumer_pairs(root, environment(defaults=True))
        results = {label + '默认': value for label, value in defaults.items()}
        actual_env = environment()
        if args.pingap_version is not None and args.pingap_commit is not None:
            # 调用方已按 CLI/环境优先级解析实际参数；打印接口须消费同一对
            # 参数，不能再被继承的旧环境否决。默认扫描仍独立清除覆盖值。
            actual_env.update(PINGAP_VERSION=args.pingap_version, PINGAP_COMMIT=args.pingap_commit)
        actual = consumer_pairs(root, actual_env)
        results.update({label + '当前环境': value for label, value in actual.items() if value != defaults[label]})
        if args.cross_repo or args.build_agent_docker:
            results['build-agent-docker versions.mk（生产默认）'] = read_pair(
                [sys.executable, str(bad / 'scripts/build/pingap_config.py'), '--root', str(bad), '--field', 'pair'],
                bad, environment(defaults=True))
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print('FAIL: 无法读取实际构建身份: ' + str(error))
        return 1
    if args.pingap_version is not None or args.pingap_commit is not None:
        results['当前请求 Pingap build args'] = (args.pingap_version, args.pingap_commit)
    if args.download_version is not None:
        results['当前请求 Pingap 下载版本'] = (args.download_version, authority[1])
    failures = []
    if args.cross_repo or args.build_agent_docker:
        for name in ('start-up.sh', 'start-up-common.sh', 'start-up-docker-extra.sh', 'start-up-k8s-extra.sh'):
            local = root / 'docker/rcoder-agent-runner' / name
            production = bad / 'build_config/rcoder-agent-runner' / name
            if not local.exists() or not production.exists() or local.read_bytes() != production.read_bytes():
                print('FAIL: 构建仓启动契约不一致: ' + name)
                failures.append(name)
    print(f'源码契约：pingap {authority[0]} @ {authority[1][:12]}')
    for label, value in results.items():
        if value != authority:
            failures.append(label)
            print(f'  [不一致] {label}: {value}')
        else:
            print(f'  [一致]   {label}')
    if failures:
        print(f'\nFAIL: {len(failures)} 处 pingap 版本漂移；构建参数必须与所选 app-cli 源码配对')
        return 1
    print('\nOK: 全部构建入口 pingap 版本一致')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
