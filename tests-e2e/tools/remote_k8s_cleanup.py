"""Opt-in remote launcher backend. Exact per-case names; never delete PVCs."""
import json
import re
import shlex
import subprocess


def kube(env, *args):
    host, context, namespace = (env.get(k, '') for k in ['TEST_K8S_SSH', 'TEST_K8S_CONTEXT', 'TEST_K8S_NS'])
    if not host or host.startswith('-') or not context or not re.fullmatch(r'rcoder-e2e-[a-z0-9][a-z0-9-]{0,35}', namespace):
        raise ValueError('Explicit remote K8s test target required')
    command = shlex.join(['kubectl', '--context', context, '-n', namespace, *args])
    return subprocess.check_output(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', host, command], text=True, timeout=90)


def validate(env):
    row = json.loads(kube(env, 'get', 'namespace', env['TEST_K8S_NS'], '-o', 'json'))
    owner = env.get('TEST_K8S_ENVIRONMENT_ID')
    if not owner or row['metadata'].get('labels', {}).get('rcoder.dev/environment') != owner:
        raise ValueError('Remote namespace ownership mismatch')


def cleanup(case_id, run_id, directory, env):
    validate(env)
    if not all(re.fullmatch(r'[0-9a-f]{32}', value) for value in [case_id, run_id]):
        raise ValueError('Invalid E2E ownership IDs')
    # Env.scoped_user adds a per-scenario suffix. Match the complete identifier
    # plus runtime labels, rather than guessing a truncated workload name.
    identifier_pattern = re.compile('ue' + case_id[:12] + r'-(?:lb|lr|ln)')
    claims = json.loads(kube(env, 'get', 'pvc', '-o', 'json'))['items']
    before = {p['metadata']['name']: p['metadata']['uid'] for p in claims}
    rows = json.loads(kube(env, 'get', 'sts,svc', '-o', 'json'))['items']
    selected = []
    for row in rows:
        labels = row['metadata'].get('labels', {})
        identifier = labels.get('rcoder.io/identifier', '')
        suffixes = ['', '-svc', '-headless'] if row['kind'] == 'Service' else ['']
        names = [prefix + identifier + suffix for prefix in ['computer-agent-runner-', 'rcoder-computer-agent-runner-'] for suffix in suffixes]
        if (identifier_pattern.fullmatch(identifier)
                and labels.get('app.kubernetes.io/managed-by') == 'rcoder-runtime'
                and labels.get('rcoder.io/service-type') == 'computer-agent-runner'
                and row['metadata']['name'] in names):
            selected.append(row)
    for row in selected:
        kube(env, 'delete', row['kind'], row['metadata']['name'], '--wait=true', '--timeout=60s')
    after = {p['metadata']['name']: p['metadata']['uid'] for p in json.loads(kube(env, 'get', 'pvc', '-o', 'json'))['items']}
    errors = ['Agent PVC disappeared or changed during cleanup'] if any(after.get(k) != v for k, v in before.items()) else []
    resources = directory / 'resources'
    resources.mkdir(exist_ok=True)
    (resources / 'k8s-cleanup.json').write_text(json.dumps({'run_id': run_id, 'case_id': case_id,
        'namespace': env['TEST_K8S_NS'], 'context': env['TEST_K8S_CONTEXT'], 'ok': not errors,
        'deleted': [r['kind'] + '/' + r['metadata']['name'] for r in selected], 'retained_pvcs': before}, indent=2))
    return errors
