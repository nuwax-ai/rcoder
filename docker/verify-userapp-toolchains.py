#!/usr/bin/env python3
"""Read actual builder/runtime toolchains; reject incompatible artifact targets."""
import argparse
import json
import subprocess


def inspect(image):
    identity = subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', image], text=True).strip()
    # Pin the probe to the resolved image, so moving a tag cannot mix evidence.
    probe = '''import json,platform,subprocess,sysconfig
java=subprocess.check_output(['java','-XshowSettings:properties','-version'],stderr=subprocess.STDOUT,text=True)
major=next(line.split('=',1)[1].strip() for line in java.splitlines() if 'java.specification.version =' in line)
print(json.dumps({'python_abi':sysconfig.get_config_var('SOABI'),'architecture':platform.machine(),'java_major':int(major)}))'''
    container = subprocess.check_output(['docker', 'create', '--network', 'none', '--entrypoint', 'python3', identity, '-c', probe], text=True).strip()
    try:
        result = subprocess.check_output(['docker', 'start', '-a', container], text=True, timeout=30)
        return {'image': image, 'id': identity, **json.loads(result)}
    finally:
        subprocess.run(['docker', 'rm', '-f', container], check=True, stdout=subprocess.DEVNULL)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--builder', default='dev-rcoder-agent-runner:latest')
    parser.add_argument('--runtime', default='dev-app-runtime:latest')
    args = parser.parse_args()
    builder, runtime = inspect(args.builder), inspect(args.runtime)
    errors = []
    for key in ('python_abi', 'architecture'):
        if builder[key] != runtime[key]:
            errors.append(f'{key} mismatch: {builder[key]} != {runtime[key]}')
    if builder['java_major'] > runtime['java_major']:
        errors.append('runtime Java is older than the builder target')
    print(json.dumps({'builder': builder, 'runtime': runtime, 'errors': errors}, indent=2))
    return int(bool(errors))


if __name__ == '__main__':
    raise SystemExit(main())
