"""SIGKILL a real Turso/file-lease worker in its committed-terminal release barrier.

Pre-kill evidence is the barrier file plus the durable operation lock file (plain
files, host-safe). The database itself is verified offline after the SIGKILL by
the lock-holding Turso observer; the SQLite engine never reads the live library.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

REPO = Path(__file__).resolve().parents[2]


def observer_records(database, report_dir):
    from turso_runtime_contract import observer_binary
    result = subprocess.run([str(observer_binary(report_dir)), str(database.resolve())],
                            capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise RuntimeError('offline observer failed: ' + result.stderr.strip()[-400:])
    section, records = None, []
    for line in result.stdout.splitlines():
        if not line:
            continue
        row = json.loads(line)
        if row.get('section'):
            section = row['section']
        elif section == 'operations':
            records.append(row)
        elif section == 'lifecycles':
            continue
        else:
            raise RuntimeError('observer record before any section marker')
    return records


def main():
    directory = Path(os.environ['E2E_REPORT_DIR']) / 'native-crash'
    directory.mkdir(parents=True, exist_ok=False)
    assertions = []
    process = None

    def record(name, ok, detail=''):
        assertions.append({'name': name, 'ok': bool(ok), 'detail': detail})
        (directory / 'assertions.json').write_text(json.dumps(assertions, indent=2))

    try:
        build = subprocess.run(['cargo', 'build', '-p', 'rcoder-e2e', '--locked', '--bin', 'lifecycle-crash-worker',
                                '--message-format=json'], cwd=REPO, capture_output=True, text=True, timeout=1800)
        (directory / 'build.jsonl').write_text(build.stdout)
        (directory / 'build.stderr').write_text(build.stderr)
        if build.returncode:
            raise RuntimeError('native crash worker build failed')
        rows = [json.loads(line) for line in build.stdout.splitlines()]
        binaries = [row['executable'] for row in rows if row.get('reason') == 'compiler-artifact'
                    and row.get('executable') and row.get('target', {}).get('name') == 'lifecycle-crash-worker']
        if len(binaries) != 1:
            raise RuntimeError('native crash worker executable missing or ambiguous')
        worker = directory / 'worker'
        shutil.copy2(binaries[0], worker)
        root = directory / 'data'
        root.mkdir()
        (root / 'payload').write_text('owned resource')
        (directory / 'identity.json').write_text(json.dumps({'run_id': os.environ['E2E_RUN_ID'],
            'case_id': os.environ['E2E_CASE_ID'], 'binary_sha256': hashlib.sha256(worker.read_bytes()).hexdigest(),
            'evidence_level': 'native_process_turso_file_lease'}, indent=2))
        with (directory / 'execute.log').open('w') as log:
            process = subprocess.Popen([str(worker), 'execute', str(root.resolve())], cwd=REPO,
                                       stdout=log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 30
            while not (root / 'release-barrier').exists():
                if process.poll() is not None:
                    raise RuntimeError('worker exited before release barrier')
                if time.monotonic() >= deadline:
                    raise RuntimeError('worker never reached terminal release barrier')
                time.sleep(0.01)
            # Pre-kill evidence: the durable lock file names the operation that
            # owns the unreceipted marker (plain files — no live-library read).
            locked_operation = (root / 'operation.lock').read_text()
            process.kill()  # Python POSIX Popen.kill sends SIGKILL, not SIGTERM.
            code = process.wait(timeout=10)
            record('Native worker terminated by SIGKILL', code == -signal.SIGKILL, str(code))
        # Offline verification (worker dead → instance lock free → observer holds it)
        records = observer_records(root / 'userapp.turso.db', os.environ['E2E_REPORT_DIR'])
        terminal = [row for row in records if row.get('operation_id') == locked_operation]
        record('Native terminal committed before release',
               len(records) == 1 and len(terminal) == 1
               and terminal[0].get('state') == 'Succeeded')
        if not assertions[-1]['ok']:
            raise RuntimeError('terminal release barrier evidence is invalid')
        verification = subprocess.run([str(worker), 'verify', str(root.resolve())], cwd=REPO,
                                      capture_output=True, text=True, timeout=60)
        (directory / 'verify.log').write_text(verification.stdout + verification.stderr)
        result = json.loads(verification.stdout) if verification.returncode == 0 else {}
        record('Native terminal retry not executed again', verification.returncode == 0 and result.get('terminal_not_reexecuted') is True)
        record('Native unreceipted marker not reclaimed', verification.returncode == 0 and result.get('marker_retained') is True)
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        record('Native crash execution', False, type(error).__name__)
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=10)
        record('Native owned process cleanup', process is None or process.poll() is not None)
    return int(not assertions or any(not item['ok'] for item in assertions))


if __name__ == '__main__':
    raise SystemExit(main())
