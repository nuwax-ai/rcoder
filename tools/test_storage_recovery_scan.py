#!/usr/bin/env python3
"""S12 PostgreSQL recovery scan plans on a uniquely owned disposable PG17 fixture.

Never connects to an existing database. No host port, credentials or global CA.
Both plans use real baseline FK/CHECK constraints and identical data/statistics.
"""
import argparse
import hashlib
import json
import pathlib
import statistics
import subprocess
import tempfile
import time
import uuid

ROOT = pathlib.Path(__file__).resolve().parents[1]
INDEX = 'userapp_operations_unfinished'
DDL = f'CREATE INDEX {INDEX} ON userapp_operations(operation_id) WHERE terminal_at_us IS NULL;'
QUERIES = {
    'unfinished_first': "SELECT * FROM userapp_operations WHERE operation_id > '' AND terminal_at_us IS NULL ORDER BY operation_id LIMIT 100",
    'unfinished_late': "SELECT * FROM userapp_operations WHERE operation_id > 'f0000000000000000000000000000000' AND terminal_at_us IS NULL ORDER BY operation_id LIMIT 100",
    'leases_first': "SELECT l.operation_id FROM userapp_operation_leases l JOIN userapp_operations o ON o.operation_id=l.operation_id AND o.app_id=l.app_id AND o.lifecycle_id=l.lifecycle_id WHERE o.terminal_at_us IS NOT NULL AND l.operation_id>'' ORDER BY l.operation_id LIMIT 100",
}


def run(args, text=None):
    result = subprocess.run(args, input=text, text=True, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(f'{args[0]} failed: {result.stderr[:2000]}')
    return result.stdout


def nodes(plan):
    yield plan
    for child in plan.get('Plans', []):
        yield from nodes(child)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true', help='Create and remove the isolated fixture')
    parser.add_argument('--image', default='postgres:17', help='An already available PG17 image')
    parser.add_argument('--history-rows', type=int, default=300_000)
    args = parser.parse_args()
    if not args.run:
        parser.error('--run is required')
    if args.history_rows < 10_000:
        parser.error('--history-rows must be at least 10000')
    schema = (ROOT / 'crates/rcoder-storage/schema/userapp-pg-v1.sql').read_text()
    if schema.count(DDL) != 1:
        raise RuntimeError('Expected exact unfinished partial index in current baseline')
    image_id = run(['docker', 'image', 'inspect', args.image, '--format', '{{.Id}}']).strip()
    report = pathlib.Path(tempfile.mkdtemp(prefix='rcoder-s12-'))
    name = 'rcoder-s12-' + uuid.uuid4().hex[:16]
    created = False

    def sql(statement):
        return run(['docker', 'exec', '-i', name, 'psql', '-h', '127.0.0.1', '-U', 'postgres',
                    '-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1'], statement)

    try:
        run(['docker', 'run', '--detach', '--name', name, '--label', 'rcoder.s12.run=' + name,
             '-e', 'POSTGRES_HOST_AUTH_METHOD=trust', args.image])
        created = True
        for _ in range(150):
            if subprocess.run(['docker', 'exec', name, 'pg_isready', '-h', '127.0.0.1',
                               '-U', 'postgres'], capture_output=True, check=False).returncode == 0:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError('Fixture TCP listener did not become ready')
        version = sql('SHOW server_version_num').strip()
        if not version.startswith('17'):
            raise RuntimeError('Fixture must use PostgreSQL 17')
        sql(schema)
        # Removing only the candidate in this owned empty fixture reproduces the
        # previous baseline; production schema and migration ledgers are untouched.
        sql(f'DROP INDEX {INDEX};')
        history = args.history_rows
        sql(f"""
INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us)
SELECT 'app'||n,'life'||n,1,'active',1,1,1 FROM generate_series(1,200)n;
INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,executor_id,step,payload_version,checkpoint_json,created_at_us,updated_at_us,terminal_at_us)
SELECT md5('history'||n),'app'||(1+(n%200)),'life'||(1+(n%200)),'start','prod','succeeded',3,repeat('a',64),'executor','done',1,'{{}}',n,n,n FROM generate_series(1,{history})n;
INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,executor_id,step,payload_version,checkpoint_json,created_at_us,updated_at_us)
SELECT md5('active'||n),'app'||n,'life'||n,'start','prod','recovery_required',2,repeat('b',64),'executor','unknown',1,'{{}}',{history}+n,{history}+n FROM generate_series(1,200)n;
INSERT INTO userapp_active_operations(app_id,lifecycle_id,prod_operation_id)
SELECT 'app'||n,'life'||n,md5('active'||n) FROM generate_series(1,200)n;
INSERT INTO userapp_operation_leases(operation_id,app_id,lifecycle_id,executor_id,request_fingerprint,receipt_version,receipt_json,created_at_us)
SELECT operation_id,app_id,lifecycle_id,executor_id,request_fingerprint,1,'{{}}',created_at_us FROM userapp_operations WHERE terminal_at_us IS NULL OR created_at_us>{history - 100};
ANALYZE;
""")
        results = {}
        rows = {}
        for phase in ('before', 'after'):
            if phase == 'after':
                sql(DDL + ' ANALYZE userapp_operations;')
            results[phase] = {}
            for key, query in QUERIES.items():
                observed = sql(query)
                if phase == 'before':
                    rows[key] = observed
                elif observed != rows[key]:
                    raise RuntimeError(f'Index changed query result: {key}')
                samples = [json.loads(sql('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' + query))[0]
                           for _ in range(5)]
                (report / f'{phase}-{key}.json').write_text(json.dumps(samples, indent=2))
                plan = samples[-1]['Plan']
                if phase == 'after' and key.startswith('unfinished'):
                    if not any(node.get('Index Name') == INDEX for node in nodes(plan)):
                        raise RuntimeError(f'Unfinished query did not use partial index: {key}')
                results[phase][key] = {
                    'median_execution_ms': statistics.median(sample['Execution Time'] for sample in samples),
                    'plan': plan,
                }
        summary = {
            'image_id': image_id, 'server_version_num': version,
            'schema_sha256': hashlib.sha256(schema.encode()).hexdigest(),
            'source_head': run(['git', '-C', str(ROOT), 'rev-parse', 'HEAD']).strip(),
            'history_rows': history, 'unfinished_rows': 200, 'lease_rows': 300,
            'index_bytes': int(sql(f"SELECT pg_relation_size('{INDEX}')").strip()),
            'queries': QUERIES, 'results': results,
            'result_equivalence': True,
        }
        (report / 'summary.json').write_text(json.dumps(summary, indent=2))
        print(report)
    finally:
        if created:
            actual = run(['docker', 'inspect', name, '--format', '{{index .Config.Labels "rcoder.s12.run"}}']).strip()
            if actual != name:
                raise RuntimeError('Fixture ownership mismatch; refusing cleanup')
            run(['docker', 'rm', '-f', '-v', name])
            (report / 'cleanup.txt').write_text('Owned container and anonymous volume removed\n')


if __name__ == '__main__':
    main()
