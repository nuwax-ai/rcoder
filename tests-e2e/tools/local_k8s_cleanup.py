"""Owned local host-K8s E2E cleanup. Never delete UserApp PVCs directly."""
import json
import os
import re
import subprocess
import time
from pathlib import Path
from urllib.parse import urlparse


def validate(env):
    namespace = env.get('TEST_K8S_NS', '')
    if (not re.fullmatch(r'rcoder-[a-z0-9-]+', namespace)
            or namespace != env.get('RCODER_K8S_NAMESPACE')):
        raise ValueError('host K8s E2E requires a dedicated matching rcoder-* namespace')
    config = env.get('KUBECONFIG', '')
    if not config or not Path(config).is_file():
        raise ValueError('host K8s E2E requires an explicit existing KUBECONFIG')
    url = urlparse(env.get('RCODER_URL', ''))
    if url.scheme != 'http' or url.hostname not in ('127.0.0.1', 'localhost'):
        raise ValueError('host K8s E2E RCODER_URL must be local HTTP')
    result = subprocess.run(['kubectl', '--kubeconfig', config, 'config', 'current-context'],
                            capture_output=True, text=True, timeout=10, check=True)
    if not result.stdout.strip():
        raise ValueError('host K8s E2E kubeconfig has no current context')
    return namespace, config, result.stdout.strip()


def kube(config, namespace, *args):
    result = subprocess.run(['kubectl', '--kubeconfig', config, '-n', namespace, *args],
                            capture_output=True, text=True, timeout=90, check=True)
    return result.stdout.strip()


def selected(config, namespace, identifier, family):
    selector = f'rcoder.io/identifier={identifier},rcoder.io/service-type={family}'
    rows = json.loads(kube(config, namespace, 'get', 'sts,svc,pod', '-l', selector, '-o', 'json'))['items']
    return [row for row in rows if row['metadata'].get('labels', {}).get('app.kubernetes.io/managed-by') == 'rcoder-runtime']


def cleanup(case_id, run_id, directory, env):
    namespace, config, context = validate(env)
    if not re.fullmatch(r'[0-9a-f]{32}', case_id):
        raise ValueError('invalid case identity')
    agent = 'ue' + case_id[:12] + '-hostk8s'
    app = 'e2e' + case_id[:12]
    errors = []
    removed = []
    if env.get('E2E_TEST_NAME') == 'host_k8s_agent_lifecycle_no_llm':
        for row in selected(config, namespace, agent, 'computer-agent-runner'):
            if row['kind'] not in ('StatefulSet', 'Service'):
                continue
            name = row['metadata']['name']
            if not name.startswith(('rcoder-computer-agent-runner-' + agent,
                                    'dev-rcoder-agent-runner-' + agent)):
                errors.append('unexpected owned agent resource name: ' + name)
                continue
            kube(config, namespace, 'delete', row['kind'], name, '--wait=true', '--timeout=60s')
            removed.append(row['kind'] + '/' + name)
        deadline = time.monotonic() + 60
        while selected(config, namespace, agent, 'computer-agent-runner') and time.monotonic() < deadline:
            time.sleep(2)
        if selected(config, namespace, agent, 'computer-agent-runner'):
            errors.append('owned agent K8s resources remain after cleanup')
    elif env.get('E2E_TEST_NAME') == 'host_k8s_userapp_dev_compute_no_llm':
        # UserApp storage and the lifecycle ledger must be purged together by
        # RCoder. A failed/interrupted operation is retained for diagnosis.
        # The workspace PVC may have no identifier label and can outlive the
        # workload, so query its owned deterministic name separately.
        pvc_name = f'rcoder-app-builder-{app}-workspace'

        def remaining():
            workloads = selected(config, namespace, app, 'user-app-builder')
            pvc = kube(config, namespace, 'get', 'pvc', pvc_name,
                       '--ignore-not-found=true', '-o', 'name')
            return workloads, pvc

        deadline = time.monotonic() + 60
        workloads, pvc = remaining()
        while (workloads or pvc) and time.monotonic() < deadline:
            time.sleep(2)
            workloads, pvc = remaining()
        if workloads or pvc:
            errors.append('owned UserApp K8s workload or PVC remains; recover the operation and purge through RCoder')
    else:
        errors.append('unknown host K8s case')
    resources = directory / 'resources'
    resources.mkdir(exist_ok=True)
    (resources / 'local-k8s-cleanup.json').write_text(json.dumps({
        'run_id': run_id, 'case_id': case_id, 'namespace': namespace,
        'context': context, 'removed': removed, 'ok': not errors, 'errors': errors,
    }, indent=2))
    return errors
