"""Run frozen lifecycle tests against a run-owned PostgreSQL 17 Compose project."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import signal
import uuid

REPO = Path(__file__).resolve().parents[2]
CASES = (
    'old_remove_preserves_replacement_and_no_resurrection',
    'delayed_clear_and_remove_preserve_reused_session',
    'reload_and_cross_replica_sync_preserve_identity',
    'container_delete_preserves_changed_association',
    'legacy_schema_backfill_is_stable',
    'flush_failure_shared_between_concurrent_callers',
        'cancelled_durable_write_is_queued_and_shutdown_waits',
)

def main():
    def cancelled(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, cancelled)
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'pg-contract'
    directory.mkdir(parents=True, exist_ok=False)
    run_id = os.environ['E2E_RUN_ID']
    project = 'rcoder-pg-' + uuid.uuid4().hex[:16]
    password = uuid.uuid4().hex
    config = directory / 'compose.json'
    config.write_text(json.dumps({'services': {'postgres': {'image': 'postgres:17',
        'environment': {'POSTGRES_USER': 'contract', 'POSTGRES_PASSWORD': '${PG_CONTRACT_PASSWORD}', 'POSTGRES_DB': 'contract'},
        'ports': ['127.0.0.1::5432'], 'volumes': ['data:/var/lib/postgresql/data'], 'labels': {'rcoder.e2e.run': run_id},
        'healthcheck': {'test': ['CMD-SHELL', 'pg_isready -U contract -d contract'], 'interval': '1s', 'timeout': '3s', 'retries': 60}}}, 'volumes': {'data': {}}}))
    (directory / 'ownership.json').write_text(json.dumps({'run_id': run_id, 'case_id': os.environ['E2E_CASE_ID'], 'project': project, 'compose_file': str(config)}))
    env = dict(os.environ, PG_CONTRACT_PASSWORD=password)
    compose = ['docker', 'compose', '-p', project, '-f', str(config)]
    assertions = []
    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': ok, 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    def command(args, **kw):
        kw.setdefault("timeout", 1800 if args[0] == "cargo" else 120)
        try:
            return subprocess.run(args, cwd=REPO, env=env, check=True, **kw)
        except subprocess.CalledProcessError as error:
            detail = str(error) + '\n' + (error.stdout or '') + '\n' + (error.stderr or '')
            (directory / 'command-failure.log').write_text(detail.replace(password, '[REDACTED]'))
            raise
    try:
        build = command(['cargo', 'test', '-p', 'rcoder-storage', '--locked', '--features', 'pg', '--lib', '--no-run', '--message-format=json'], capture_output=True, text=True)
        artifacts = [json.loads(line) for line in build.stdout.splitlines()]
        binary = next(row['executable'] for row in artifacts if row.get('reason') == 'compiler-artifact' and row.get('executable'))
        frozen = directory / 'pg-tests'
        shutil.copy2(binary, frozen)
        (directory / 'binary-sha256.txt').write_text(hashlib.sha256(frozen.read_bytes()).hexdigest())
        (directory / 'build.jsonl').write_text(build.stdout.replace(password, '[REDACTED]'))
        listing = command([str(frozen), '--list', '--format=terse'], capture_output=True, text=True).stdout
        names = {line.removesuffix(': test') for line in listing.splitlines() if line.endswith(': test')}
        expected = ['pg::project_store::lifecycle_tests::lifecycle_contract_' + case for case in CASES]
        if not set(expected) <= names:
            raise RuntimeError('required PG lifecycle test missing from executable')
        command(compose + ['up', '-d', '--wait', '--wait-timeout', '90'], stdout=subprocess.DEVNULL)
        cid = command(compose + ['ps', '-q', 'postgres'], capture_output=True, text=True).stdout.strip()
        identity = command(['docker', 'inspect', '--format', '{{.Id}} {{.Image}} {{index .Config.Labels "rcoder.e2e.run"}}', cid], capture_output=True, text=True).stdout.strip()
        if not identity.endswith(' ' + run_id):
            raise RuntimeError('PG resource ownership mismatch')
        (directory / 'identity.txt').write_text(identity)
        address = command(compose + ['port', 'postgres', '5432'], capture_output=True, text=True).stdout.strip()
        env.update(RCODER_PG_TEST_DSN=f'postgres://contract:{password}@{address}/contract', RCODER_PG_TEST_STRICT='1')
        record('PG17 isolated environment ready', True, identity)
        for name, case in zip(expected, CASES):
            result = subprocess.run([str(frozen), name, '--exact', '--nocapture'], cwd=REPO, env=env, capture_output=True, text=True, timeout=120)
            log = (result.stdout + result.stderr).replace(password, '[REDACTED]')
            (directory / (case + '.log')).write_text(log)
            record('PG ' + case, result.returncode == 0 and '1 passed; 0 failed; 0 ignored' in log)
    except Exception as error:
        record('PG contract execution', False, str(error).replace(password, '[REDACTED]'))
    finally:
        try:
            logs = subprocess.run(compose + ['logs', '--no-color'], env=env, capture_output=True, text=True, timeout=20)
            (directory / 'postgres.log').write_text((logs.stdout + logs.stderr).replace(password, '[REDACTED]'))
        except subprocess.TimeoutExpired:
            record('PG diagnostics collected before cleanup', False, 'log collection timed out')
        try:
            cleanup = subprocess.run(compose + ['down', '-v', '--remove-orphans'], env=env, capture_output=True, text=True, timeout=60)
            record('PG owned project cleanup', cleanup.returncode == 0, cleanup.stderr.replace(password, '[REDACTED]'))
        except subprocess.TimeoutExpired:
            record('PG owned project cleanup', False, 'cleanup timed out: ' + project)
    return int(not assertions or any(not item['ok'] for item in assertions))

if __name__ == '__main__':
    raise SystemExit(main())
