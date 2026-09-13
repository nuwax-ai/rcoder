"""Read-only SQLite Compose configuration gate; never starts or removes services."""
import argparse
import json
import os
from pathlib import Path
import subprocess


def validate(config, expected_directory=None, named_volume=False):
    service = config.get('services', {}).get('rcoder')
    if not isinstance(service, dict):
        raise ValueError('rcoder service is missing')
    env = service.get('environment', {})
    if env.get('RCODER_USERAPP_STORAGE_BACKEND') != 'sqlite':
        raise ValueError('userApp SQLite backend must be explicit')
    if env.get('RCODER_USERAPP_SQLITE_PATH') != '/app/data/userapp.sqlite3':
        raise ValueError('unexpected SQLite database path')
    if service.get('deploy', {}).get('replicas', 1) != 1 or service.get('scale', 1) != 1:
        raise ValueError('SQLite Compose supports one rcoder replica')
    mounts = service.get('volumes', [])
    data = [item for item in mounts if item.get('target') == '/app/data']
    if len(data) != 1 or data[0].get('read_only', False):
        raise ValueError('one writable entire-directory data mount is required')
    if any(item.get('target', '').startswith('/app/data/') for item in mounts):
        raise ValueError('nested mounts cannot replace SQLite files or sidecars')
    mount = data[0]
    if named_volume:
        if mount.get('type') != 'volume' or mount.get('source') != 'rcoder-userapp-data':
            raise ValueError('project-scoped SQLite named volume is required')
        definition = config.get('volumes', {}).get('rcoder-userapp-data')
        if not isinstance(definition, dict) or definition.get('external', False):
            raise ValueError('SQLite volume must not be external')
        if definition.get('name') != config.get('name', '') + '_rcoder-userapp-data':
            raise ValueError('SQLite volume name must be scoped to the Compose project')
    else:
        if expected_directory is None or not Path(expected_directory).is_absolute():
            raise ValueError('expected host data directory must be absolute')
        if mount.get('type') != 'bind' or os.path.normpath(mount.get('source', '')) != os.path.normpath(str(expected_directory)):
            raise ValueError('SQLite bind source does not match the expected data directory')
    return {'backend': 'sqlite', 'database': env['RCODER_USERAPP_SQLITE_PATH'],
            'mount_type': mount['type'], 'mount_source': mount['source'],
            'evidence_level': 'compose_configuration_only'}


def inspect(compose, data_directory=None, named_volume=False):
    compose = Path(compose).absolute()
    env = os.environ.copy()
    # Explicit CLI input prevents an unrelated shell override from changing the
    # baseline being checked. No credentials or full resolved config are printed.
    env['RCODER_DATA_DIR'] = str(compose.parent / 'data' / 'rcoder')
    if data_directory is not None:
        if not Path(data_directory).is_absolute():
            raise ValueError('data directory override must be absolute')
        env['RCODER_DATA_DIR'] = str(data_directory)
    args = ['docker', 'compose', '--project-name', 'rcoder-sqlite-contract', '-f', str(compose)]
    if named_volume:
        args += ['-f', str(compose.with_name('docker-compose.sqlite-volume.yml'))]
    args += ['config', '--format', 'json']
    try:
        result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=30)
    except subprocess.TimeoutExpired as error:
        raise ValueError('Compose configuration resolution timed out') from error
    if result.returncode:
        # Compose diagnostics can contain interpolated credentials.
        raise ValueError('Compose configuration resolution failed; inspect configuration locally')
    config = json.loads(result.stdout)
    expected = Path(data_directory) if data_directory is not None else compose.parent / 'data' / 'rcoder'
    return validate(config, expected, named_volume)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('compose', type=Path)
    parser.add_argument('--data-directory', type=Path)
    parser.add_argument('--named-volume', action='store_true')
    options = parser.parse_args()
    try:
        print(json.dumps(inspect(options.compose, options.data_directory, options.named_volume), indent=2))
    except (ValueError, OSError) as error:
        parser.exit(1, f'SQLite Compose contract failed: {error}\n')
