#!/usr/bin/env python3
"""Observe and pin a runtime base before pairing it with newly built app-cli."""
import argparse
import json
import re
import subprocess
import sys


def docker_output(arguments):
    result = subprocess.run(['docker', *arguments], capture_output=True, text=True)
    if result.returncode:
        raise ValueError('runtime base observation failed: docker ' + ' '.join(arguments[:3]))
    return result.stdout.strip()


def verify_base(image, pingap_version, pingap_commit, node_version, platform=None, registry=False):
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', pingap_version):
        raise ValueError('invalid expected Pingap version')
    if not re.fullmatch(r'[0-9a-f]{40}', pingap_commit):
        raise ValueError('runtime base needs an exact Pingap commit')
    if node_version != '22.23.2':
        raise ValueError('runtime base Node must retain 22.23.2')
    if registry:
        # Pin the manifest/index digest, which remote BuildKit can resolve too.
        description = docker_output(['buildx', 'imagetools', 'inspect', image])
        matches = re.findall(r'^Digest:\s*(sha256:[0-9a-f]{64})\s*$', description, re.MULTILINE)
        if len(matches) != 1:
            raise ValueError('runtime base has no unique immutable registry digest')
        repository = image.split('@', 1)[0]
        prefix, slash, name = repository.rpartition('/')
        repository = (prefix + slash if slash else '') + name.split(':', 1)[0]
        identity = repository + '@' + matches[0]
    else:
        identity = docker_output(['image', 'inspect', '--format', '{{.Id}}', image])
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', identity):
            raise ValueError('runtime base has no immutable local image ID; load the selected base first')
    if platform and ',' in platform:
        receipts = [verify_base(identity, pingap_version, pingap_commit, node_version, item, registry=True)
                    for item in platform.split(',')]
        if any(receipt['image_id'] != identity for receipt in receipts):
            raise ValueError('runtime registry base identity changed during architecture observations')
        return {'image': image, 'image_id': identity, 'platforms': receipts}
    run = ['run', '--rm', '--network', 'none']
    if platform:
        run += ['--platform', platform]
    def observe(binary, argument):
        return docker_output([*run, '--entrypoint', binary, identity, argument])
    short = observe('/usr/local/bin/pingap', '-V')
    long = observe('/usr/local/bin/pingap', '--version')
    node = observe('/usr/local/bin/node', '--version')
    if short != 'pingap ' + pingap_version:
        raise ValueError('runtime base Pingap short version mismatch: ' + short)
    expected_long = f'pingap {pingap_version} ({pingap_commit}, tls=openssl)'
    if long != expected_long:
        raise ValueError('runtime base Pingap commit/TLS identity mismatch: ' + long)
    if node != 'v' + node_version:
        raise ValueError('runtime base Node version mismatch: ' + node)
    return {'image': image, 'image_id': identity, 'pingap_short': short,
            'pingap_long': long, 'node': node, 'platform': platform}


def pin_local_image(identity):
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', identity):
        raise ValueError('local base needs an observed immutable image ID')
    # BuildKit treats a raw sha256:<id> FROM as a registry name. Use a separate
    # content-addressed tag and verify it points to the observed local object.
    selected = 'local/rcoder-verified-base:' + identity.split(':', 1)[1]
    docker_output(['tag', identity, selected])
    observed = docker_output(['image', 'inspect', '--format', '{{.Id}}', selected])
    if observed != identity:
        raise ValueError('verified local runtime base tag identity changed')
    return selected


def pin_base(command, version, commit, node_version, registry=False):
    found = []
    for index, part in enumerate(command):
        if part == '--build-arg' and index + 1 < len(command) and command[index + 1].startswith('BASE_IMAGE='):
            found.append((index + 1, 'BASE_IMAGE='))
        elif part.startswith('--build-arg=BASE_IMAGE='):
            found.append((index, '--build-arg=BASE_IMAGE='))
    if len(found) != 1:
        raise ValueError('runtime build requires one explicit BASE_IMAGE')
    index, prefix = found[0]
    platform = next((command[i + 1] for i, value in enumerate(command[:-1]) if value == '--platform'), None)
    receipt = verify_base(command[index][len(prefix):], version, commit, node_version, platform, registry)
    receipt['build_reference'] = receipt['image_id'] if registry else pin_local_image(receipt['image_id'])
    command[index] = prefix + receipt['build_reference']
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--pingap-version', required=True)
    parser.add_argument('--pingap-commit', required=True)
    parser.add_argument('--node-version', required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(verify_base(args.image, args.pingap_version, args.pingap_commit, args.node_version), sort_keys=True))
        return 0
    except (ValueError, OSError) as error:
        print('runtime base validation: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
