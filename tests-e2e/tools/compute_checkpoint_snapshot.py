#!/usr/bin/env python3
"""Read a private compute checkpoint with the stopped fixture's own Turso engine.

No live database, copied SQLite snapshot, schema initialization or controller
restart is permitted here. The observer takes the original directory lock.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import uuid

DATABASE = '/app/data/userapp.turso.db'
OBSERVER = '/app/bin/userapp-db-observer'


def command(*args):
    result = subprocess.run(args, check=True, capture_output=True, text=True, timeout=90)
    return result.stdout.strip()


def inspect_controller(row, *, controller, image, run_id, case_id, volume):
    labels = row.get('Config', {}).get('Labels') or {}
    project = labels.get('com.docker.compose.project', '')
    state = row.get('State') or {}
    mounts = [m for m in row.get('Mounts', []) if m.get('Destination') == '/app/data']
    if (row.get('Id') != controller or row.get('Image') != image
            or state.get('Running') is not False or state.get('Status') != 'exited'
            or labels.get('com.docker.compose.service') != 'rcoder'
            or labels.get('rcoder.e2e.run') != run_id or labels.get('rcoder.e2e.case') != case_id
            or not re.fullmatch(r'rcoder-cfailure-[0-9a-f]{16}', project)
            or volume != project + '_compute-data'
            or len(mounts) != 1 or mounts[0].get('Type') != 'volume'
            or mounts[0].get('Name') != volume):
        raise ValueError('checkpoint observer requires the exact stopped owned controller and data volume')
    return {'id': controller, 'image': image, 'project': project,
            'started_at': state.get('StartedAt'), 'finished_at': state.get('FinishedAt'),
            'restart_count': row.get('RestartCount'), 'mount': mounts[0]}


def validate_volume(row, *, run_id, case_id, volume):
    labels = row.get('Labels') or {}
    if (row.get('Name') != volume or labels.get('rcoder.e2e.run') != run_id
            or labels.get('rcoder.e2e.case') != case_id):
        raise ValueError('checkpoint data volume is not owned by this case')


def validate_record(record, args):
    if (not isinstance(record, dict) or record.get('app_id') != args.app_id
            or record.get('lifecycle_id') != args.lifecycle_id
            or record.get('operation_id') != args.operation_id):
        raise ValueError('offline observer did not return the requested operation')
    return record


def snapshot(args):
    build = json.loads(Path(args.build_receipt).read_text())
    role = build.get('rcoder') or {}
    image, expected = role.get('image_id'), role.get('observer_sha256')
    if (build.get('version') != 1 or not isinstance(image, str)
            or not re.fullmatch(r'sha256:[0-9a-f]{64}', image)
            or not isinstance(expected, str) or not re.fullmatch(r'[0-9a-f]{64}', expected)):
        raise ValueError('immutable controller image and compiled observer checksum are required')
    ownership = dict(controller=args.controller, image=image, run_id=args.run_id,
                     case_id=args.case_id, volume=args.data_volume)

    def controller():
        rows = json.loads(command('docker', 'inspect', args.controller))
        if len(rows) != 1:
            raise ValueError('expected one physical controller')
        return inspect_controller(rows[0], **ownership)

    def no_writers():
        running = command('docker', 'ps', '-q', '--no-trunc', '--filter', 'volume=' + args.data_volume)
        if running:
            raise ValueError('checkpoint volume still has a running container')

    before = controller()
    volumes = json.loads(command('docker', 'volume', 'inspect', args.data_volume))
    if len(volumes) != 1:
        raise ValueError('expected one owned data volume')
    validate_volume(volumes[0], run_id=args.run_id, case_id=args.case_id, volume=args.data_volume)
    no_writers()
    actual = command('docker', 'run', '--rm', '--network', 'none', '--entrypoint', 'sha256sum',
                     image, OBSERVER).split()[0]
    if actual != expected:
        raise ValueError('observer image binary differs from its actual compile receipt')
    name = 'rcoder-compute-observer-' + uuid.uuid4().hex
    # Turso requires write access for its own engine bookkeeping. Read-only here
    # describes the operation repository transaction, not a read-only mount.
    raw = command('docker', 'run', '--rm', '--name', name, '--network', 'none',
                  '--label', 'rcoder.e2e.run=' + args.run_id,
                  '--label', 'rcoder.e2e.case=' + args.case_id,
                  '--mount', 'type=volume,source=' + args.data_volume + ',target=/app/data',
                  '--entrypoint', OBSERVER, image, DATABASE, '--compute-operation',
                  args.app_id, args.operation_id)
    record = validate_record(json.loads(raw), args)
    no_writers()
    after = controller()
    if before != after:
        raise ValueError('controller changed during offline observation')
    return {'version': 1, 'read_only': True, 'controller_stopped': True,
            'observer_sha256': actual, 'controller': after, 'record': record,
            'raw_record_sha256': hashlib.sha256(raw.encode()).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('build-receipt', 'run-id', 'case-id', 'controller', 'data-volume',
                'app-id', 'lifecycle-id', 'operation-id', 'report'):
        parser.add_argument('--' + key, required=True)
    args = parser.parse_args()
    result = snapshot(args)
    path = Path(args.report)
    # The enclosing strict case already owns this private directory.
    with path.open('x') as stream:
        os.chmod(path, 0o600)
        json.dump(result, stream, indent=2)
        stream.write('\n')
    print(json.dumps({'report': str(path), 'success': True}))


if __name__ == '__main__':
    main()
