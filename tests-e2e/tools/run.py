#!/usr/bin/env python3
"""Strict E2E launcher. Each selected libtest case owns an isolated report directory."""
import atexit
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import uuid
from cleanup import cleanup_case
from contracts import REQUIRED

ROOT = Path(__file__).resolve().parents[1]
REPO = ROOT.parent
GROUPS = {
    'userapp': ['compose_userapp', 'compose_userapp_dev', 'compose_userapp_build_rules', 'compose_userapp_faults', 'compose_userapp_deploy'],
    'compose': ['compose_sse', 'compose_session', 'compose_userapp', 'compose_userapp_dev', 'compose_userapp_build_rules', 'compose_webchat'],
    'deploy': ['compose_userapp_deploy'],
    'k8s': ['k8s_lb'],
}


def validate_reports(directory, scenario=None):
    files = list(directory.glob('*.jsonl'))
    errors = []
    observed = set()
    if not files:
        return ['no scenario reports produced']
    for path in files:
        try:
            lines = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
            ends = [line for line in lines if line.get('kind') == 'scenario_end']
            if sum(line.get('kind') == 'scenario_begin' for line in lines) != 1:
                errors.append(f'{path.name}: missing or duplicate scenario begin')
            if len(ends) != 1 or ends[0].get('verdict') != 'pass':
                errors.append(f'{path.name}: missing, duplicate, or unsuccessful terminal')
            assertions = [line for line in lines if line.get('kind') == 'assert' and line.get('level') == 'hard']
            observed.update(a.get("name") for a in assertions if a.get("ok") is True)
            if not assertions:
                errors.append(f'{path.name}: no hard assertions')
            if any(line.get('ok') is not True for line in assertions):
                errors.append(f'{path.name}: unsuccessful hard assertion')
            if len(ends) == 1 and (ends[0].get('hard_pass') != sum(a.get('ok') is True for a in assertions) or ends[0].get('hard_fail') != sum(a.get('ok') is not True for a in assertions)):
                errors.append(f'{path.name}: assertion counters disagree with records')
            if len(ends) == 1 and lines[-1] != ends[0]:
                errors.append(f'{path.name}: records after terminal')
        except (ValueError, OSError) as exc:
            errors.append(f'{path.name}: unreadable report: {exc}')
    missing = REQUIRED.get(scenario, set()) - observed
    if missing:
        errors.append("missing required acceptance steps: " + ", ".join(sorted(missing)))
    return errors


def output(*args):
    return subprocess.check_output(args, cwd=REPO, text=True).strip()


def source_fingerprint():
    digest = hashlib.sha256()
    paths = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=REPO).split(b'\0')
    for raw in sorted(set(paths)):
        if not raw:
            continue
        path = REPO / os.fsdecode(raw)
        digest.update(raw + b'\0')
        if path.is_file():
            digest.update(path.read_bytes())
        else:
            digest.update(b'<missing>')
    return digest.hexdigest()


def container_identities():
    try:
        ids = output('docker', 'ps', '-aq').split()
        if not ids:
            return []
        # Explicit format intentionally excludes environment variables and credentials.
        rows = output('docker', 'inspect', '--format', '{{json .Id}} {{json .Name}} {{json .Image}} {{json .Config.Image}}', *ids)
        return rows.splitlines()
    except (OSError, subprocess.CalledProcessError) as exc:
        return {'unavailable': type(exc).__name__}


def main():
    def cancelled(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, cancelled)
    parser = argparse.ArgumentParser()
    parser.add_argument('--group', choices=GROUPS, default='userapp')
    parser.add_argument('--suite', default=os.environ.get('E2E_SUITE', ''))
    parser.add_argument('--filter', default=os.environ.get('E2E_FILTER', ''))
    parser.add_argument('--ignored', action='store_true')
    args = parser.parse_args()
    suites = args.suite.split(',') if args.suite else GROUPS[args.group]
    known = set(sum(GROUPS.values(), []))
    if not suites or any(s not in known for s in suites):
        parser.error('unknown or empty suite selection')
    run_id = uuid.uuid4().hex
    run = ROOT / 'reports' / run_id
    run.mkdir(parents=True)
    env = dict(os.environ, E2E_RUN_ID=run_id, E2E_STRICT='1')
    manifest = {'run_id': run_id, 'head': output('git', 'rev-parse', 'HEAD'),
                'worktree_sha256': source_fingerprint(),
                'containers_before': container_identities(),
                'suites': suites, 'filter': args.filter, 'planned': [], 'results': []}
    def persist():
        completed = {(row['suite'], row['test']) for row in manifest['results']}
        unfinished = [{**case, 'verdict': 'aborted', 'errors': ['no completed process result']} for case in manifest['planned'] if (case['suite'], case['test']) not in completed]
        snapshot = {**manifest, 'results': manifest['results'] + unfinished}
        (run / 'summary.json').write_text(json.dumps(snapshot, indent=2))
    atexit.register(persist)
    persist()
    inventory = manifest['containers_before']
    if not isinstance(inventory, list) and args.group != 'k8s':
        manifest['infrastructure_error'] = 'Docker inventory unavailable before acceptance'
        return 2
    existing_ids = [json.loads(row.split()[0]) for row in inventory] if isinstance(inventory, list) else []
    env['E2E_EXISTING_CONTAINER_IDS'] = json.dumps(existing_ids)
    command = ['cargo', 'test', '-p', 'rcoder-e2e', '--locked', '--no-run', '--message-format=json']
    for suite in suites:
        command += ['--test', suite]
    build = subprocess.run(command, cwd=REPO, env=env, text=True, stdout=subprocess.PIPE)
    (run / 'build.jsonl').write_text(build.stdout)
    if build.returncode:
        manifest['infrastructure_error'] = f'build exited {build.returncode}'
        return build.returncode
    if source_fingerprint() != manifest['worktree_sha256']:
        manifest['infrastructure_error'] = 'source changed while compiling acceptance binaries'
        return 2
    executables = {}
    for line in build.stdout.splitlines():
        event = json.loads(line)
        if event.get('reason') == 'compiler-artifact' and event.get('executable'):
            name = event['target']['name']
            frozen = run / 'bin' / name
            frozen.parent.mkdir(exist_ok=True)
            shutil.copy2(event['executable'], frozen)
            executables[name] = str(frozen)
            manifest.setdefault('test_binary_sha256', {})[name] = hashlib.sha256(frozen.read_bytes()).hexdigest()
    for suite in suites:
        executable = executables[suite]
        listing = subprocess.check_output([executable, '--list', '--format=terse'], text=True)
        for line in listing.splitlines():
            if not line.endswith(': test'):
                continue
            name = line.removesuffix(': test')
            # Gate-only smoke tests deliberately skip; these are not acceptance scenarios.
            if name.startswith('gate_') or (args.filter and args.filter not in name):
                continue
            manifest['planned'].append({'suite': suite, 'test': name, 'executable': executable})
    (run / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    if not manifest['planned']:
        manifest['infrastructure_error'] = 'selection matched no scenarios'
        print(f'ERROR: selection matched no scenarios; report: {run}', file=sys.stderr)
        return 1
    for case in manifest['planned']:
        case_dir = run / case['suite'] / case['test']
        case_dir.mkdir(parents=True)
        case_id = uuid.uuid4().hex
        case['case_id'] = case_id
        persist()
        case_env = dict(env, E2E_REPORT_DIR=str(case_dir), E2E_TEST_NAME=case['test'], E2E_CASE_ID=case_id)
        cmd = [case['executable'], case['test'], '--exact', '--test-threads=1', '--nocapture']
        if args.ignored:
            cmd += ['--include-ignored']
        with (case_dir / 'process.log').open('w') as log:
            process = subprocess.Popen(cmd, cwd=REPO, env=case_env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            interrupted = False
            try:
                exit_code = process.wait(timeout=3600)
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                interrupted = isinstance(error, KeyboardInterrupt)
                exit_code = 130 if interrupted else 124
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait()
        errors = validate_reports(case_dir, case["test"])
        errors.extend(cleanup_case(case_id, run_id, case_dir, existing_ids))
        for cleanup in (case_dir / 'resources').glob('*-cleanup.json'):
            try:
                if json.loads(cleanup.read_text()).get('ok') is not True:
                    errors.append(f'cleanup failed: {cleanup.name}')
            except (OSError, ValueError):
                errors.append(f'unreadable cleanup evidence: {cleanup.name}')
        manifest['containers_after'] = container_identities()
        if exit_code:
            errors.append(f'libtest exit {exit_code}')
        verdict = 'fail' if errors else 'pass'
        manifest['results'].append({**case, 'verdict': verdict, 'errors': errors})
        persist()
        print(f'{verdict}: {case["suite"]}::{case["test"]}', flush=True)
        if interrupted:
            return 130
    manifest['worktree_sha256_after'] = source_fingerprint()
    if manifest['worktree_sha256_after'] != manifest['worktree_sha256']:
        manifest['infrastructure_error'] = 'source changed during acceptance run'
        print(f'ERROR: source changed during run; report: {run}', file=sys.stderr)
        return 2
    print(f'Report: {run}')
    return int(any(case['verdict'] != 'pass' for case in manifest['results']))


if __name__ == '__main__':
    sys.exit(main())
