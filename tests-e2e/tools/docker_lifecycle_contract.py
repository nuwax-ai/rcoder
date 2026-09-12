"""Exercise the actual Docker runtime's immutable deletion receipts."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

from build_context_contract import run as build_context_run

REPO = Path(__file__).resolve().parents[2]
TEST = 'stale_deletion_receipt_preserves_replacement_container_and_volume'

def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'docker-lifecycle'
    directory.mkdir(parents=True, exist_ok=True)
    assertions = []
    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))
    try:
        image = os.environ.get('E2E_RUNTIME_IMAGE', 'dev-app-runtime:latest')
        image_id = subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', image], text=True, timeout=30).strip()
        cid = subprocess.check_output(['docker', 'compose', '-f', 'docker/docker-compose.yml', 'ps', '-q', 'rcoder'], cwd=REPO, text=True, timeout=30).strip()
        if not cid:
            raise RuntimeError('local rcoder Compose container is required for runtime network discovery')
        command = ['cargo', 'test', '-p', 'docker_manager', '--locked', '--test', 'docker_deletion_identity', '--no-run', '--message-format=json']
        build = subprocess.run(command, cwd=REPO, capture_output=True, text=True, timeout=1800)
        (directory / 'build.jsonl').write_text(build.stdout)
        (directory / 'build.log').write_text(build.stderr)
        if build.returncode:
            raise RuntimeError('Docker lifecycle test build failed')
        events = [json.loads(line) for line in build.stdout.splitlines()]
        source = next(row['executable'] for row in events if row.get('reason') == 'compiler-artifact' and row.get('executable'))
        binary = directory / 'docker-tests'
        shutil.copy2(source, binary)
        (directory / 'binary-sha256.txt').write_text(hashlib.sha256(binary.read_bytes()).hexdigest())
        env = dict(os.environ, RCODER_DOCKER_IDENTITY_TEST='1', RCODER_DOCKER_TEST_IMAGE=image, HOSTNAME=cid)
        result = subprocess.run([str(binary), TEST, '--exact', '--nocapture'], cwd=REPO, env=env, capture_output=True, text=True, timeout=180)
        log = result.stdout + result.stderr
        (directory / 'test.log').write_text(log)
        record('Q01 real Docker test executed', result.returncode == 0 and '1 passed; 0 failed; 0 ignored' in log)
        rows = [json.loads(line.partition('identity_evidence=')[2]) for line in log.splitlines() if line.startswith('identity_evidence=')]
        record('Q01 Docker replacement and volume identity proved', len(rows) == 1 and rows[0]['old_container_id'] != rows[0]['new_container_id'] and rows[0]['marker'] == 'replacement', json.dumps(rows))
        (directory / 'image.json').write_text(json.dumps({'image': image, 'image_id': image_id, 'rcoder_container_id': cid}))
    except Exception as error:
        record('Docker lifecycle execution', False, str(error))
    for item in build_context_run(Path(os.environ['E2E_REPORT_DIR']) / 'build-context', os.environ['E2E_RUN_ID'], os.environ['E2E_CASE_ID']):
        record(item['name'], item['ok'], item['detail'])
    return int(not assertions or any(not item['ok'] for item in assertions))

if __name__ == '__main__':
    raise SystemExit(main())
