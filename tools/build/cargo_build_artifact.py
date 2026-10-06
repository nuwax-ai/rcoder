#!/usr/bin/env python3
"""只从本次成功 Cargo JSON 构建输出发布指定二进制，不猜 target 目录。"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def build_artifact(manifest: Path, binary: str, target: str, output: Path, command: list[str]) -> int:
    if command[:1] != ['cargo'] or '--message-format=json' not in command:
        raise ValueError('产物捕获必须运行 Cargo，并启用 --message-format=json')
    manifest = manifest.resolve(strict=True)
    process = subprocess.Popen(command, stdout=subprocess.PIPE, text=True)
    candidates = []
    invalid_output = False
    finished = False
    for line in process.stdout:
        try:
            event = json.loads(line)
        except ValueError:
            invalid_output = True
            continue
        if not isinstance(event, dict):
            invalid_output = True
            continue
        if event.get('reason') == 'compiler-message':
            rendered = event.get('message', {}).get('rendered')
            if isinstance(rendered, str):
                print(rendered, file=sys.stderr, end='')
        elif event.get('reason') == 'build-finished':
            finished = event.get('success') is True
        elif event.get('reason') == 'compiler-artifact':
            artifact_target = event.get('target', {})
            artifact_manifest = event.get('manifest_path')
            if (artifact_target.get('name') == binary
                    and artifact_target.get('kind') == ['bin']
                    and isinstance(artifact_manifest, str)
                    and Path(artifact_manifest).resolve() == manifest
                    and event.get('profile', {}).get('test') is False):
                candidates.append(event.get('executable'))
    status = process.wait()
    process.stdout.close()
    if status:
        return status if status > 0 else 128 - status
    if invalid_output or not finished:
        raise ValueError('本轮 Cargo 输出无效或缺少成功 build-finished 证据')
    if len(candidates) != 1 or not isinstance(candidates[0], str):
        raise ValueError(f'本轮 Cargo 必须返回唯一 {binary} compiler-artifact executable（{manifest}）')
    executable = Path(candidates[0]).resolve(strict=True)
    if (not executable.is_file() or executable.stat().st_size == 0
            or executable.parent.name != 'release'
            or executable.parent.parent.name != target.split('.')[0]):
        raise ValueError(f'本轮 {binary} 产物缺失、为空或与请求的 release target 不一致')
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = None
    try:
        with tempfile.NamedTemporaryFile(prefix='.' + output.name + '.', dir=output.parent, delete=False) as temporary:
            staging = Path(temporary.name)
        shutil.copy2(executable, staging)
        os.replace(staging, output)
    finally:
        if staging is not None:
            staging.unlink(missing_ok=True)
    print(f'产物来源：{executable}')
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest-path', required=True, type=Path)
    parser.add_argument('--bin', required=True)
    parser.add_argument('--target', required=True)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    try:
        return build_artifact(args.manifest_path, args.bin, args.target, args.output, command)
    except (OSError, ValueError) as error:
        print(f'❌ Cargo 产物捕获失败：{error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
