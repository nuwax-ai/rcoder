"""Require actual paired Pingap/Node versions from the exact remote base digests."""
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'build'))
from pingap_identity import parse_pingap_version, source_identity

NODE_VERSION = '22.23.2'
BASES = ('COMPUTER_BASE', 'RUNTIME_BASE')


def full_identity(output):
    match = re.fullmatch(r'pingap ([0-9A-Za-z.+-]+) \(([0-9a-f]{40}), tls=(openssl|rustls)\)', output.strip())
    if not match:
        raise ValueError('unexpected Pingap full version identity: ' + repr(output))
    return {'version': parse_pingap_version('pingap ' + match.group(1)),
            'commit': match.group(2), 'tls': match.group(3)}


def check_overrides(config, source):
    for key, expected in [('PINGAP_VERSION', source['version']), ('PINGAP_COMMIT', source['commit']), ('NODE_VERSION', NODE_VERSION)]:
        override = config.get(key, '')
        if override and override != expected:
            raise ValueError(key + ' override disagrees with source identity')


def inspect_bases(config, bases, source):
    observations = {}
    for key in BASES:
        image = bases[key]
        if not re.fullmatch(r'.+@sha256:[0-9a-f]{64}', image):
            raise ValueError('Pingap base must use an exact digest: ' + key)
        commands = {name: ['docker', 'run', '--rm', '--pull=always', '--platform', 'linux/amd64',
                           '--network', 'none', '--entrypoint', name, image, '--version']
                    for name in ('pingap', 'node')}
        commands['pingap_full'] = list(commands['pingap'])
        commands['pingap'][-1] = '-V'
        pingap_output = config.ssh(commands['pingap'], timeout=180).strip()
        version = parse_pingap_version(pingap_output)
        if version != source['version']:
            raise ValueError(f'{key} Pingap {version} disagrees with source {source["version"]}')
        full_output = config.ssh(commands['pingap_full'], timeout=180).strip()
        observed = full_identity(full_output)
        if observed['version'] != version or observed['commit'] != source['commit']:
            raise ValueError(key + ' actual Pingap commit disagrees with paired source')
        node_output = config.ssh(commands['node'], timeout=180).strip()
        if node_output != 'v' + NODE_VERSION:
            raise ValueError(f'{key} Node must remain {NODE_VERSION}, observed {node_output!r}')
        observations[key] = {'image': image, 'pingap_version': version,
                             'node_version': NODE_VERSION, 'pingap_stdout': pingap_output,
                             'node_stdout': node_output, 'pingap_full_stdout': full_output,
                             'pingap_commit': observed['commit'], 'tls': observed['tls'], 'commands': commands}
    return {'status': 'verified', 'source': source, 'node_version': NODE_VERSION, 'bases': observations}


def validate_receipt(receipt, bases=None, expected=None):
    expected = source_identity() if expected is None else expected
    if not isinstance(receipt, dict) or receipt.get('status') != 'verified' or receipt.get('source') != expected or receipt.get('node_version') != NODE_VERSION or set(receipt.get('bases', {})) != set(BASES):
        raise ValueError('verified Pingap build identity required')
    for key in BASES:
        record = receipt['bases'][key]
        image = record.get('image', '')
        if not re.fullmatch(r'.+@sha256:[0-9a-f]{64}', image) or (bases is not None and image != bases.get(key)):
            raise ValueError('Pingap receipt base digest mismatch: ' + key)
        observed = full_identity(record.get('pingap_full_stdout', ''))
        if observed['version'] != expected['version'] or observed['commit'] != expected['commit'] or record.get('pingap_commit') != expected['commit'] or record.get('tls') != observed['tls']:
            raise ValueError('Pingap receipt lacks matching actual commit/TLS: ' + key)
        if record.get('pingap_version') != expected['version'] or parse_pingap_version(record.get('pingap_stdout', '')) != expected['version'] or record.get('node_version') != NODE_VERSION or record.get('node_stdout') != 'v' + NODE_VERSION:
            raise ValueError('Pingap receipt lacks matching actual observations: ' + key)
    return expected
