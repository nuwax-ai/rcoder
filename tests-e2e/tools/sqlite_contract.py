"""Execute frozen userApp SQLite component contracts; no platform or AI required."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
from storage_contract_cases import SQLITE_TARGETS, passed_exactly_one

REPO = Path(__file__).resolve().parents[2]


def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'sqlite-contract'
    directory.mkdir(parents=True, exist_ok=False)
    assertions = []

    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': ok, 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))

    try:
        build = subprocess.run(['cargo', 'test', '-p', 'rcoder-storage', '--locked',
                                '--features', 'sqlite', '--lib', '--no-run', '--message-format=json'],
                               cwd=REPO, capture_output=True, text=True, timeout=1800)
        (directory / 'build.jsonl').write_text(build.stdout)
        (directory / 'build.stderr').write_text(build.stderr)
        if build.returncode:
            raise RuntimeError('SQLite contract compilation failed')
        artifacts = [json.loads(line) for line in build.stdout.splitlines()]
        binaries = [row['executable'] for row in artifacts if row.get('reason') == 'compiler-artifact'
                    and row.get('executable') and row.get('target', {}).get('name') == 'rcoder_storage']
        if len(binaries) != 1:
            raise RuntimeError('expected exactly one storage contract executable')
        frozen = directory / 'sqlite-tests'
        shutil.copy2(binaries[0], frozen)
        (directory / 'identity.json').write_text(json.dumps({
            'run_id': os.environ['E2E_RUN_ID'], 'case_id': os.environ['E2E_CASE_ID'],
            'binary_sha256': hashlib.sha256(frozen.read_bytes()).hexdigest(),
            'evidence_level': 'sqlite_component', 'planned_cases': list(SQLITE_TARGETS),
        }, indent=2))
        listing = subprocess.run([str(frozen), '--list', '--format=terse'], cwd=REPO,
                                 capture_output=True, text=True, timeout=30, check=True).stdout
        names = {line.removesuffix(': test') for line in listing.splitlines() if line.endswith(': test')}
        missing = set(SQLITE_TARGETS.values()) - names
        record('SQLite frozen cases present', not missing, ', '.join(sorted(missing)))
        if missing:
            return 1
        for case, target in SQLITE_TARGETS.items():
            result = subprocess.run([str(frozen), target, '--exact', '--nocapture'],
                                    cwd=REPO, capture_output=True, text=True, timeout=120)
            log = result.stdout + result.stderr
            (directory / (case + '.log')).write_text(log)
            record('SQLite ' + case, passed_exactly_one(result.returncode, log))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        record('SQLite contract execution', False, type(error).__name__ + ': ' + str(error))
    return int(not assertions or any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
