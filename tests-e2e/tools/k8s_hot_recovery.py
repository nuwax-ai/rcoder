"""Independent real-K8s hot-recovery faults, restricted to the test namespace.

The default 81-step UserApp contract remains unchanged. All application traffic,
artifacts, app-cli status, leases and configuration operations are real; only
artifact download timing and this invocation's exact K8s metadata are faulted.
"""
import base64
import datetime
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shlex
import subprocess
import threading
import time
import uuid


REQUIRED = frozenset({
    'cluster_ready', 'concurrent_ensure', 'concurrent_error_contract', 'ensure_retry',
    'builder_identity', 'lifecycle_active', 'cross_replica_files',
    'build_A', 'artifact_A', 'build_B', 'artifact_B', 'build_C', 'artifact_C',
    'build_D', 'artifact_D', 'worker_artifact_contract', 'hot_recovery_seed_data',
    'hot_recovery_cold_A', 'hot_recovery_fixture_owned',
    'hot_expired_original_admitted', 'hot_expired_exact_lease_fault',
    'hot_expired_original_store_fenced', 'hot_expired_owner_B',
    'hot_expired_original_terminal', 'hot_expired_env_honest',
    'hot_expired_slot_and_lease_released', 'hot_expired_fresh_C',
    'hot_expired_late_coordinator_fenced',
    'hot_convergence_original_admitted', 'hot_convergence_exact_config_fault',
    'hot_convergence_owner_D', 'hot_convergence_original_failed',
    'hot_convergence_newer_config_retained', 'hot_convergence_slot_and_lease_released',
    'hot_convergence_fresh_request', 'hot_convergence_failure_preserved',
    'hot_recovery_no_artifact_replay', 'hot_recovery_pod_preserved',
    'hot_recovery_data_preserved', 'hot_recovery_fixture_cleanup',
    'cleanup', 'baseline_preserved',
})

FIXTURE_LABEL = 'rcoder.dev/e2e-run'
FIXTURE_CASE_LABEL = 'rcoder.dev/e2e-case'
FIXTURE_CASE = 'hot-recovery-fixture'
RELEASE_ENV = ('APP_DEPLOY_URL', 'APP_RELEASE_ID', 'APP_DEPLOY_SHA256',
               'APP_DEPLOY_OPERATION_ID')
TERMINAL = frozenset(('Succeeded', 'Failed'))
HOT_REQUEST_TIMEOUT_SECONDS = 300


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def metadata_identity(row):
    meta = row.get('metadata') or {}
    if not all(isinstance(meta.get(key), str) and meta[key]
               for key in ('name', 'uid', 'resourceVersion')):
        raise AssertionError('Observed K8s resource identity is incomplete')
    return {key: meta[key] for key in ('name', 'uid', 'resourceVersion')}


def exact_patch(row, changes):
    meta = metadata_identity(row)
    return [{'op': 'test', 'path': '/metadata/uid', 'value': meta['uid']},
            {'op': 'test', 'path': '/metadata/resourceVersion', 'value': meta['resourceVersion']},
            *changes]


def original_operation(row, app, lifecycle, request_id):
    if (not isinstance(row, dict) or row.get('app_id') != app
            or row.get('lifecycle_id') != lifecycle or row.get('request_id') != request_id
            or row.get('kind') != 'StartDeployment' or row.get('scope') != 'Prod'
            or not isinstance(row.get('operation_id'), str) or not row['operation_id']):
        raise AssertionError('Hot operation differs from the original public request identity')
    return row


def original_lease(lease, app, lifecycle, operation_id, association, captured=None):
    """Associate only this run's newly admitted request with its exact mutex.

    Ordinary hot operations acquire a legacy random-token mutex *before* SQL
    admission assigns the public operation/executor. Its missing redundant
    execution annotations are normal; HTTP does not expose private checkpoint
    executor/fingerprint values, so this tool never manufactures them.
    """
    if (association.get('lease_absent_before_request') is not True
            or association.get('prod_slot_empty_before_request') is not True
            or association.get('app_id') != app or association.get('lifecycle_id') != lifecycle
            or association.get('operation_id') != operation_id
            or association.get('owner_operation_id') != operation_id
            or not association.get('request_id') or not association.get('pod_uid')
            or not association.get('request_started_at')):
        raise AssertionError('Hot Lease has no exact fresh request/owner association')
    meta = metadata_identity(lease)
    labels = lease['metadata'].get('labels') or {}
    annotations = lease['metadata'].get('annotations') or {}
    spec = lease.get('spec') or {}
    created = lease['metadata'].get('creationTimestamp')
    if not isinstance(created, str) or not created:
        raise AssertionError('Original hot Lease creation identity is missing')
    datetime.datetime.fromisoformat(created.replace('Z', '+00:00'))
    # Match production holder_token's priority, including rejection when a
    # present higher-priority token field is empty rather than falling through.
    token = next((annotations[key] for key in ('rcoder.io/lease-token',
                  'rcoder.io/operation-id', 'rcoder.io/legacy-operation-id')
                  if key in annotations), None)
    if (meta['name'] != 'rcoder-operation-prod-' + app
            or labels.get('rcoder.io/operation-app') != app
            or labels.get('rcoder.io/operation-family') != 'user-app'
            or not isinstance(token, str) or not token
            or annotations.get('rcoder.io/compute-lease') == 'true'
            or lease['metadata'].get('deletionTimestamp')
            or spec.get('leaseDurationSeconds') != 60
            or not isinstance(spec.get('renewTime'), str)):
        raise AssertionError('Refusing to fault an unconfirmed or replaced original hot Lease')
    expected_optional = {'rcoder.io/application-id': app, 'rcoder.io/lifecycle-id': lifecycle,
                         'rcoder.io/operation-id': operation_id}
    if any(annotations.get(key) not in (None, '', expected)
           for key, expected in expected_optional.items()):
        raise AssertionError('Original hot Lease contains contradictory execution fields')
    legacy = annotations.get('rcoder.io/legacy-operation-id')
    if legacy:
        if token != legacy or spec.get('holderIdentity') != 'legacy:' + legacy:
            raise AssertionError('Original legacy hot Lease token/holder changed')
        shape = 'fresh-legacy-before-admission'
    else:
        executor = annotations.get('rcoder.io/executor-id')
        if (annotations.get('rcoder.io/lifecycle-id') != lifecycle
                or annotations.get('rcoder.io/operation-id') != operation_id
                or not isinstance(executor, str) or not executor or token != operation_id
                or spec.get('holderIdentity') != executor + ':' + operation_id):
            raise AssertionError('Original context-bound hot Lease execution identity changed')
        shape = 'context-bound'
    fingerprint = annotations.get('rcoder.io/request-fingerprint')
    if fingerprint not in (None, '') and (not isinstance(fingerprint, str)
                                          or re.fullmatch(r'[0-9a-fA-F]{64}', fingerprint) is None):
        raise AssertionError('Original hot Lease request fingerprint is malformed')
    identity = {**meta, 'holder_token': token, 'holder_identity': spec.get('holderIdentity'),
                'creation_timestamp': created, 'shape': shape,
                'execution_annotations': {key: annotations.get(key) for key in (
                    *expected_optional, 'rcoder.io/executor-id', 'rcoder.io/request-fingerprint')}}
    if captured is not None and any(identity[key] != captured[key] for key in identity
                                    if key != 'resourceVersion'):
        raise AssertionError('Original hot Lease changed after exact initial capture')
    return identity


def fixture_owned(row, run, baseline_uids, expected_uid=None):
    meta = metadata_identity(row)
    labels = row['metadata'].get('labels') or {}
    annotations = row['metadata'].get('annotations') or {}
    if (row.get('kind') not in ('Pod', 'Service', 'ConfigMap')
            or meta['name'] != 'rcoder-e2e-hot-' + run.id[:20]
            or row['metadata'].get('namespace') != run.args.namespace
            or meta['uid'] in baseline_uids or expected_uid is not None and meta['uid'] != expected_uid
            or labels.get(FIXTURE_LABEL) != run.id or labels.get(FIXTURE_CASE_LABEL) != FIXTURE_CASE
            or labels.get('app.kubernetes.io/managed-by') != 'rcoder-e2e'
            or annotations.get('rcoder.io/application-id') != run.app
            or annotations.get('rcoder.io/lifecycle-id') != run.lifecycle_id):
        raise AssertionError('Refusing mutation of a non-owned hot artifact fixture')
    return meta


def env_projection(cm):
    data = cm.get('data') or {}
    return {**metadata_identity(cm), 'release_env': {key: data.get(key) for key in RELEASE_ENV},
            'all_env_sha256': digest(data)}


def env_matches(cm, artifact, request_release_id, operation_id):
    data = cm.get('data') or {}
    return (data.get('APP_DEPLOY_URL') == artifact['url']
            and data.get('APP_DEPLOY_SHA256') == artifact['sha256']
            and data.get('APP_RELEASE_ID') == request_release_id
            and data.get('APP_DEPLOY_OPERATION_ID') == operation_id)



def runtime_fixture_image(pod):
    images = pod.get('images') or []
    if len(images) != 1:
        raise AssertionError('Cannot identify the single actual app-runtime image')
    # Status.image can be a locally preloaded config ID, which is not a
    # registry reference. Prefer the actual immutable pullable imageID.
    for value in (images[0].get('imageID'), images[0].get('image')):
        if isinstance(value, str):
            reference = value.removeprefix('docker-pullable://')
            if re.fullmatch(r'[A-Za-z0-9._:/-]+@sha256:[0-9a-f]{64}', reference):
                return reference
    raise AssertionError('Original app-runtime has no observed immutable pullable image reference')

def historical_terminal_preserved(before, after):
    return (isinstance(before, dict) and isinstance(after, dict)
            and before.get('state') in TERMINAL and type(before.get('revision')) is int
            and type(after.get('revision')) is int and after['revision'] >= before['revision']
            and {key: value for key, value in before.items() if key != 'revision'}
            == {key: value for key, value in after.items() if key != 'revision'})


def kube_json_input(run, verb, body, *args):
    """Structured stdin, normal TLS, no shell interpolation of body or secrets."""
    command = ['kubectl']
    if getattr(run.args, 'context', None):
        command += ['--context', run.args.context]
    if getattr(run.args, 'kube_server', None):
        command += ['--server', run.args.kube_server]
    command += ['-n', run.args.namespace, verb, *args]
    result = subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
                             run.args.ssh, shlex.join(command)],
                            input=json.dumps(body), text=True, capture_output=True,
                            check=True, timeout=90)
    return json.loads(result.stdout)


class ArtifactFixture:
    def __init__(self, run, image, artifacts):
        if run.args.namespace != 'nuwax-k8s-test':
            raise ValueError('Hot-recovery fault injection is restricted to nuwax-k8s-test')
        self.run = run
        self.name = 'rcoder-e2e-hot-' + run.id[:20]
        self.image = image
        self.artifacts = artifacts
        self.created = {}
        self.intents = []
        self.baseline_uids = {row['uid'] for row in run.baseline}

    def documents(self):
        run = self.run
        meta = {'name': self.name, 'namespace': run.args.namespace,
                'labels': {FIXTURE_LABEL: run.id, FIXTURE_CASE_LABEL: FIXTURE_CASE,
                           'app.kubernetes.io/managed-by': 'rcoder-e2e'},
                'annotations': {'rcoder.io/application-id': run.app,
                                'rcoder.io/lifecycle-id': run.lifecycle_id}}
        server = Path(__file__).with_name('k8s_hot_fixture.py').read_text()
        cm = {'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': meta,
              'data': {'server.py': server,
                       'identity.json': json.dumps({'run_id': run.id, 'app': run.app,
                                                    'lifecycle': run.lifecycle_id})},
              'binaryData': {version + '.zip': base64.b64encode(body).decode()
                             for version, body in self.artifacts.items()}}
        service = {'apiVersion': 'v1', 'kind': 'Service', 'metadata': meta,
                   'spec': {'selector': {FIXTURE_LABEL: run.id, FIXTURE_CASE_LABEL: FIXTURE_CASE},
                            'ports': [{'port': 8080, 'targetPort': 8080}]}}
        pod = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': meta,
               'spec': {'restartPolicy': 'Never', 'automountServiceAccountToken': False,
                        'terminationGracePeriodSeconds': 5,
                        'securityContext': {'runAsUser': 1000, 'runAsGroup': 1000,
                                            'runAsNonRoot': True, 'fsGroup': 1000,
                                            'seccompProfile': {'type': 'RuntimeDefault'}},
                        'containers': [{'name': 'artifact-barrier', 'image': self.image,
                                        'command': ['python3', '-u', '/fixture/server.py'],
                                        'imagePullPolicy': 'IfNotPresent',
                                        'securityContext': {'allowPrivilegeEscalation': False,
                                                            'readOnlyRootFilesystem': True,
                                                            'capabilities': {'drop': ['ALL']}},
                                        'ports': [{'containerPort': 8080}],
                                        'resources': {'requests': {'cpu': '25m', 'memory': '32Mi'},
                                                      'limits': {'memory': '128Mi'}},
                                        'volumeMounts': [{'name': 'artifacts', 'mountPath': '/fixture', 'readOnly': True},
                                                         {'name': 'control', 'mountPath': '/control'}],
                                        'readinessProbe': {'httpGet': {'path': '/health', 'port': 8080},
                                                           'periodSeconds': 1, 'timeoutSeconds': 1}}],
                        'volumes': [{'name': 'artifacts', 'configMap': {'name': self.name}},
                                    {'name': 'control', 'emptyDir': {}}]}}
        return [cm, service, pod]

    def create(self):
        for doc in self.documents():
            kind = doc['kind'].lower()
            if self.run.kube('get', kind, self.name, '--ignore-not-found', '-o', 'json').strip():
                raise AssertionError('Hot fixture name already exists; refusing replacement')
            self.intents.append(kind)
            self.run.save('hot-fixture-create-intents.json', self.intents)
            row = kube_json_input(self.run, 'create', doc, '-f', '-', '-o', 'json')
            self.created[kind] = fixture_owned(row, self.run, self.baseline_uids)
            self.run.save('hot-fixture-identities.json', self.created)
        self.run.kube('wait', '--for=condition=Ready', 'pod/' + self.name, '--timeout=120s')
        self.run.check('hot_recovery_fixture_owned', len(self.created) == 3
                       and all(identity['uid'] not in self.baseline_uids for identity in self.created.values()),
                       self.created)

    def current(self, kind):
        text = self.run.kube('get', kind, self.name, '--ignore-not-found', '-o', 'json').strip()
        if not text:
            return None
        row = json.loads(text)
        fixture_owned(row, self.run, self.baseline_uids,
                      (self.created.get(kind) or {}).get('uid'))
        return row

    def control(self, action, version=None):
        if action not in ('state', 'release') or action == 'release' and version not in ('B', 'D'):
            raise ValueError('Unknown fixture control action or artifact')
        if self.current('pod') is None:
            raise AssertionError('Original hot artifact fixture Pod is absent')
        code = '''import json,pathlib,sys
identity=json.loads(pathlib.Path('/fixture/identity.json').read_text())
assert identity['run_id']==sys.argv[1] and identity['app']==sys.argv[2] and identity['lifecycle']==sys.argv[3]
action,version=sys.argv[4:6]
if action=='release':
 assert version in ('B','D')
 pathlib.Path('/control',version+'.release').write_text(identity['run_id'])
path=pathlib.Path('/control/state.json')
print(path.read_text() if path.exists() else json.dumps({'counts':{'B':0,'D':0},'events':[]}))'''
        state = json.loads(self.run.kube('exec', self.name, '--', 'python3', '-c', code,
                                        self.run.id, self.run.app, self.run.lifecycle_id,
                                        action, version or ''))
        if self.current('pod') is None:
            raise AssertionError('Hot fixture Pod changed during control observation')
        self.run.save('hot-fixture-state.json', state)
        return state

    def url(self, version):
        return 'http://' + self.name + '.' + self.run.args.namespace + '.svc:8080/' + version + '.zip'

    def cleanup(self):
        errors = []
        if 'pod' in self.intents:
            try:
                if self.current('pod') is not None:
                    for version in ('B', 'D'):
                        self.control('release', version)
            except Exception as error:
                errors.append('release fixture barriers: ' + repr(error))
        for kind in ('pod', 'service', 'configmap'):
            if kind not in self.intents:
                continue
            try:
                row = self.current(kind)
                if row is None:
                    continue
                meta = fixture_owned(row, self.run, self.baseline_uids,
                                     (self.created.get(kind) or {}).get('uid'))
                options = {'apiVersion': 'v1', 'kind': 'DeleteOptions',
                           'preconditions': {'uid': meta['uid'], 'resourceVersion': meta['resourceVersion']}}
                plural = {'pod': 'pods', 'service': 'services', 'configmap': 'configmaps'}[kind]
                path = '/api/v1/namespaces/' + self.run.args.namespace + '/' + plural + '/' + self.name
                kube_json_input(self.run, 'delete', options, '--raw', path, '-f', '-')
                self.run.kube('wait', '--for=delete', kind + '/' + self.name, '--timeout=60s')
                if self.run.kube('get', kind, self.name, '--ignore-not-found', '-o', 'json').strip():
                    raise AssertionError('Exact fixture resource still exists after deletion')
            except Exception as error:
                errors.append(kind + ' fixture cleanup: ' + repr(error))
        self.run.check('hot_recovery_fixture_cleanup', not errors,
                       {'errors': errors, 'owned': self.created, 'pvc_delete_requests': 0})


class HotRecovery:
    def __init__(self, run):
        self.run = run
        self.pod = None
        self.claims = None
        self.marker = None
        self.threads = []
        self.timeline = []
        self.deployment_uid = None
        self.config_identity = None
        self.associations = {}

    def event(self, stage, **values):
        self.timeline.append({'stage': stage, 'monotonic': time.monotonic(), **values})
        self.run.save('hot-recovery-timeline.json', self.timeline)

    def physical_pod(self):
        current = self.run.prod_pod()
        if self.pod is not None and current['uid'] != self.pod['uid']:
            raise AssertionError('Hot recovery must not replace the original runtime Pod')
        return current

    def owner(self):
        pod = self.physical_pod()
        body = json.loads(self.run.kube('exec', pod['name'], '--', 'curl', '--silent',
            '--show-error', '--fail', '--noproxy', '*', '--connect-timeout', '2',
            '--max-time', '5', 'http://127.0.0.1:3010/v1/deploy/status'))
        if self.physical_pod()['uid'] != pod['uid']:
            raise AssertionError('Original hot owner Pod changed during status observation')
        data = body.get('data')
        if not isinstance(data, dict) or data.get('protocol_version') != 4:
            raise AssertionError('Original owner lacks the actual protocol-4 status contract')
        self.run.save('hot-owner-latest.json', {'pod_uid': pod['uid'], 'data': data})
        return data

    def config(self):
        deployment = json.loads(self.run.kube('get', 'deployment', 'rcoder-app-' + self.run.app, '-o', 'json'))
        annotations = deployment['metadata'].get('annotations') or {}
        if (annotations.get('rcoder.io/application-id') != self.run.app
                or annotations.get('rcoder.io/lifecycle-id') != self.run.lifecycle_id):
            raise AssertionError('Original runtime Deployment identity is not this application lifecycle')
        refs = [source['configMapRef']['name']
                for container in deployment['spec']['template']['spec']['containers']
                for source in container.get('envFrom') or [] if source.get('configMapRef')]
        if len(refs) != 1:
            raise AssertionError('Runtime has an ambiguous env ConfigMap reference')
        cm = json.loads(self.run.kube('get', 'configmap', refs[0], '-o', 'json'))
        deployment_identity = metadata_identity(deployment)
        cm_identity = metadata_identity(cm)
        if (self.deployment_uid is not None and deployment_identity['uid'] != self.deployment_uid
                or self.config_identity is not None
                and (cm_identity['name'], cm_identity['uid']) != self.config_identity):
            raise AssertionError('Original hot deployment or env ConfigMap physical identity changed')
        self.deployment_uid = deployment_identity['uid']
        self.config_identity = (cm_identity['name'], cm_identity['uid'])
        self.run.save('hot-env-latest.json', {'deployment': metadata_identity(deployment),
                                             'config': env_projection(cm)})
        return cm

    def operation(self, request_id):
        row = self.run.operation_view(request_id)
        return None if row is None else original_operation(row, self.run.app, self.run.lifecycle_id, request_id)

    def start_holder(self, artifact, label):
        request_id = 'hot-recovery-' + label + '-' + self.run.id[:16]
        if any(thread.is_alive() for thread, _result, _request in self.threads):
            raise AssertionError('Another owned hot HTTP request is still active')
        def idle():
            rows = self.run.api('/api/v1/userapp/' + self.run.app + '/operations/current',
                                base=self.run.entries[-1], timeout=15)
            if not isinstance(rows, list):
                raise AssertionError('Current operation response is not a list')
            return {'prod': [row for row in rows if row.get('scope') == 'Prod'], 'lease': self.lease()}
        before = self.run.poll(idle, lambda observed: not observed['prod'] and observed['lease'] is None, 90)
        self.associations[request_id] = {
            'request_id': request_id, 'app_id': self.run.app, 'lifecycle_id': self.run.lifecycle_id,
            'lease_absent_before_request': before['lease'] is None,
            'prod_slot_empty_before_request': not before['prod'],
            'request_started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'request_started_monotonic': time.monotonic(), 'before': before}
        self.run.save('hot-association-' + request_id + '.json', self.associations[request_id])
        result = {}
        def call():
            try:
                status, envelope = self.run.request('/api/v1/userapp/' + self.run.app + '/start',
                    {'user_id': self.run.user, 'request_id': request_id,
                     **artifact, 'deploy_mode': 'hot'}, base=self.run.entries[0], timeout=HOT_REQUEST_TIMEOUT_SECONDS)
                result.update(status=status, envelope=envelope)
            except Exception as error:
                result['error'] = repr(error)
            self.event(label + '-http-ended', request_id=request_id)
        thread = threading.Thread(target=call, daemon=True)
        self.threads.append((thread, result, request_id))
        thread.start()
        self.event(label + '-http-started', request_id=request_id)
        return thread, result, request_id

    def admitted_at_barrier(self, fixture, version, holder):
        thread, _result, request_id = holder
        self.run.poll(lambda: fixture.control('state'),
                      lambda state: state['counts'].get(version, 0) >= 1, 90)
        operation = self.operation(request_id)
        owner = self.owner()
        op = owner.get('operation') or {}
        admitted = (thread.is_alive() and operation is not None
                    and operation.get('state') == 'Running' and operation.get('step') == 'hot_execution'
                    and op.get('operation_id') == operation['operation_id']
                    and bool(op.get('deployment_generation_id'))
                    and op.get('phase') == 'deploying' and op.get('request_release_id'))
        prefix = 'hot_expired' if version == 'B' else 'hot_convergence'
        self.run.check(prefix + '_original_admitted', bool(admitted),
                       {'operation': operation, 'owner': owner, 'holder_http_pending': thread.is_alive()})
        self.associations[request_id].update(operation_id=operation['operation_id'],
            owner_operation_id=op['operation_id'], pod_uid=self.physical_pod()['uid'],
            first_owner_observed_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
        self.run.save('hot-association-' + request_id + '.json', self.associations[request_id])
        self.event(version + '-download-barrier', operation_id=operation['operation_id'])
        return operation, owner

    def running_owner(self, operation, artifact_release):
        def matched(data):
            op = data.get('operation') or {}
            return (data.get('phase') == 'running' and op.get('operation_id') == operation['operation_id']
                    and op.get('phase') == 'running' and op.get('deploy_stage') == 'succeeded'
                    and op.get('persisted') is True and bool(op.get('deployment_generation_id'))
                    and op.get('artifact_release_id') == artifact_release)
        return self.run.poll(self.owner, matched, 180)

    def terminal(self, request_id):
        return self.run.poll(lambda: self.operation(request_id),
                             lambda row: row is not None and row.get('state') in TERMINAL, 180)

    def lease(self):
        text = self.run.kube('get', 'lease', 'rcoder-operation-prod-' + self.run.app,
                             '--ignore-not-found', '-o', 'json').strip()
        return json.loads(text) if text else None

    def slot_released(self, check_name):
        path = '/api/v1/userapp/' + self.run.app + '/operations/current'
        def observe():
            operations = self.run.api(path, base=self.run.entries[-1], timeout=15)
            if not isinstance(operations, list):
                raise AssertionError('Current operation response is not a list')
            return {'prod': [row for row in operations if row.get('scope') == 'Prod'],
                    'lease': self.lease()}
        observed = self.run.poll(observe, lambda row: not row['prod'] and row['lease'] is None, 90)
        self.run.check(check_name, not observed['prod'] and observed['lease'] is None, observed)

    def normal_hot(self, artifact, version, check_name):
        request_id = 'hot-recovery-normal-' + uuid.uuid4().hex[:16]
        result = self.run.api('/api/v1/userapp/' + self.run.app + '/start',
            {'user_id': self.run.user, 'request_id': request_id, **artifact, 'deploy_mode': 'hot'},
            base=self.run.entries[-1], timeout=300)
        operation = self.operation(request_id)
        cm = self.config()
        self.run.check(check_name, operation is not None and operation.get('state') == 'Succeeded'
                       and env_matches(cm, artifact, result.get('release_id'), operation['operation_id'])
                       and self.run.content(self.run.id + '-' + version)
                       and self.physical_pod()['uid'] == self.pod['uid'],
                       {'operation': operation, 'env': env_projection(cm), 'response': result})
        return operation

    def seed_data(self):
        filename = 'hot-sentinel-' + self.run.id[:12] + '.txt'
        status, response = self.run.request('/api/v1/userapp/generate-file',
            {'app_id': self.run.app, 'user_id': self.run.user, 'file_name': filename, 'content': self.run.id},
            base=self.run.entries[0], headers={'X-App-Id': self.run.app})
        if status != 200 or response.get('success') is not True:
            raise AssertionError('Hot recovery sentinel creation failed')
        self.run.dev_git('init', entry=self.run.entries[0])
        committed = self.run.dev_git('commit', {'files': [filename], 'message': 'e2e hot recovery ' + self.run.id},
                                     self.run.entries[0])
        commit = committed.get('commit')
        if not isinstance(commit, str) or re.fullmatch(r'[0-9a-f]{40}', commit) is None:
            raise AssertionError('Hot recovery sentinel has no actual Git commit')
        self.dev_marker = (filename, commit, self.run.dev_data_proof(filename, commit))
        self.run.check('hot_recovery_seed_data', True, self.dev_marker[2])

    def seed_production_volume(self):
        pod = json.loads(self.run.kube('get', 'pod', self.pod['name'], '-o', 'json'))
        if pod['metadata']['uid'] != self.pod['uid']:
            raise AssertionError('Runtime Pod changed before data witness')
        claims = dict(self.run.app_pvc_identities(self.run.inventory()))
        expected = 'rcoder-app-' + self.run.app + '-workspace'
        if expected not in claims:
            raise AssertionError('Original runtime PVC is absent')
        volumes = {v['name']: v.get('persistentVolumeClaim', {}).get('claimName')
                   for v in pod['spec']['volumes']}
        mounts = []
        for container in pod['spec']['containers']:
            pgdata = next((e.get('value') for e in container.get('env') or [] if e.get('name') == 'PGDATA'), None)
            if not pgdata:
                continue
            for mount in container.get('volumeMounts') or []:
                if (volumes.get(mount['name']) == expected and not mount.get('readOnly', False)
                        and PurePosixPath(pgdata).is_relative_to(PurePosixPath(mount['mountPath']))):
                    mounts.append(mount['mountPath'])
        if len(mounts) != 1:
            raise AssertionError('Cannot select the actual writable PG-data PVC mount for data witness')
        self.marker = str(PurePosixPath(mounts[0]) / ('.e2e-hot-' + self.run.id))
        self.claims = claims
        code = "import pathlib,sys,hashlib; p=pathlib.Path(sys.argv[1]); p.write_text(sys.argv[2]); print(hashlib.sha256(p.read_bytes()).hexdigest())"
        actual = self.run.kube('exec', self.pod['name'], '--', 'python3', '-c', code,
                               self.marker, self.run.id).strip()
        if actual != hashlib.sha256(self.run.id.encode()).hexdigest():
            raise AssertionError('Original runtime PVC marker did not reach actual disk')
        self.event('production-data-witness', pod_uid=self.pod['uid'], claim_uid=claims[expected],
                   marker_sha256=actual)

    def data_preserved(self):
        filename, commit, before = self.dev_marker
        after = self.run.dev_data_proof(filename, commit)
        claims = dict(self.run.app_pvc_identities(self.run.inventory()))
        code = 'import pathlib,sys,hashlib; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())'
        actual = self.run.kube('exec', self.physical_pod()['name'], '--', 'python3', '-c', code, self.marker).strip()
        self.run.check('hot_recovery_data_preserved', before == after and claims == self.claims
                       and actual == hashlib.sha256(self.run.id.encode()).hexdigest(),
                       {'before': before, 'after': after, 'claims': claims,
                        'production_marker_sha256': actual})

    def expired_lease_case(self, fixture, artifact_b, release_b, artifact_c):
        before = self.config()
        holder = self.start_holder(artifact_b, 'expired')
        operation, accepted = self.admitted_at_barrier(fixture, 'B', holder)
        lease = self.lease()
        if lease is None:
            raise AssertionError('Original admitted hot Lease is absent')
        association = self.associations[holder[2]]
        captured = original_lease(lease, self.run.app, self.run.lifecycle_id,
                                  operation['operation_id'], association)
        self.run.save('hot-original-lease-capture.json', {'association': association, 'capture': captured,
            'first_capture_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'timestamp_use': 'diagnostic; identity relies on before-absence/sole request/owner match and exact UID/token'})
        # A fresh renewal gives the existing 5s scanner a window before the
        # original coordinator's next 20s heartbeat. This is expiry injection,
        # not a host crash or stopped-renewal simulation.
        def fresh(row):
            if row is None:
                raise AssertionError('Original hot Lease disappeared before expiry injection')
            original_lease(row, self.run.app, self.run.lifecycle_id, operation['operation_id'], association, captured)
            timestamp = datetime.datetime.fromisoformat(row['spec']['renewTime'].replace('Z', '+00:00'))
            return (datetime.datetime.now(datetime.timezone.utc) - timestamp).total_seconds() < 5
        lease = self.run.poll(self.lease, fresh, 25)
        stale = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=1)).isoformat(timespec='microseconds').replace('+00:00', 'Z')
        patch = exact_patch(lease, [{'op': 'replace', 'path': '/spec/renewTime', 'value': stale}])
        fault = json.loads(self.run.kube('patch', 'lease', lease['metadata']['name'], '--type=json',
                                        '-p', json.dumps(patch), '-o', 'json'))
        original_lease(fault, self.run.app, self.run.lifecycle_id, operation['operation_id'], association, captured)
        self.run.check('hot_expired_exact_lease_fault', fault['metadata']['uid'] == lease['metadata']['uid']
                       and fault['spec']['renewTime'] == stale and fault['spec']['leaseDurationSeconds'] == 60,
                       {'before': lease, 'after': fault, 'patch': patch,
                        'fault_kind': 'expired exact original Lease, coordinator remains alive'})
        fenced = self.run.poll(lambda: self.operation(holder[2]),
            lambda row: row is not None and row.get('state') == 'RecoveryRequired'
            and row.get('step') == 'hot_execution', 15)
        self.run.check('hot_expired_original_store_fenced', fenced['operation_id'] == operation['operation_id']
                       and holder[0].is_alive() and fenced['revision'] > operation['revision'], fenced)
        fixture.control('release', 'B')
        owner_b = self.running_owner(operation, release_b)
        self.run.check('hot_expired_owner_B', self.run.content(self.run.id + '-B'), owner_b)
        terminal = self.terminal(holder[2])
        self.run.check('hot_expired_original_terminal', terminal['operation_id'] == operation['operation_id']
                       and (terminal['state'] == 'Succeeded'
                            or terminal['state'] == 'Failed' and bool(terminal.get('error_message'))), terminal)
        cm = self.config()
        target_matches = env_matches(cm, artifact_b, accepted['operation']['request_release_id'], operation['operation_id'])
        self.run.check('hot_expired_env_honest', target_matches if terminal['state'] == 'Succeeded'
                       else cm.get('data') == before.get('data'),
                       {'terminal': terminal, 'before': env_projection(before), 'after': env_projection(cm),
                        'exact_target_matches': target_matches})
        self.slot_released('hot_expired_slot_and_lease_released')
        self.normal_hot(artifact_c, 'C', 'hot_expired_fresh_C')
        self.join_holder(holder)
        observed = self.operation(holder[2])
        self.run.check('hot_expired_late_coordinator_fenced', historical_terminal_preserved(terminal, observed),
                       {'before': terminal, 'after': observed, 'original_http': holder[1]})

    def convergence_case(self, fixture, artifact_d, release_d, artifact_c):
        before = self.config()
        holder = self.start_holder(artifact_d, 'convergence')
        operation, _accepted = self.admitted_at_barrier(fixture, 'D', holder)
        current = self.config()
        if (metadata_identity(current) != metadata_identity(before)
                or current.get('data') != before.get('data')):
            raise AssertionError('Original env snapshot changed before the controlled configuration fault')
        key = 'rcoder.dev/hot-recovery-' + self.run.id[:12]
        annotations = {**(current['metadata'].get('annotations') or {}), key: self.run.id}
        patch = exact_patch(current, [{'op': 'add', 'path': '/metadata/annotations', 'value': annotations}])
        newer = json.loads(self.run.kube('patch', 'configmap', current['metadata']['name'], '--type=json',
                                        '-p', json.dumps(patch), '-o', 'json'))
        self.run.check('hot_convergence_exact_config_fault', newer['metadata']['uid'] == current['metadata']['uid']
                       and newer['metadata']['resourceVersion'] != current['metadata']['resourceVersion']
                       and newer.get('data') == current.get('data')
                       and newer['metadata']['annotations'].get(key) == self.run.id,
                       {'before': env_projection(current), 'after': env_projection(newer), 'patch': patch,
                        'fault_kind': 'exact original env revision advanced; business env unchanged'})
        fixture.control('release', 'D')
        owner_d = self.running_owner(operation, release_d)
        self.run.check('hot_convergence_owner_D', self.run.content(self.run.id + '-D'), owner_d)
        terminal = self.terminal(holder[2])
        self.run.check('hot_convergence_original_failed', terminal['operation_id'] == operation['operation_id']
                       and terminal['state'] == 'Failed' and terminal.get('step') == 'hot_converging'
                       and bool(terminal.get('error_code')) and bool(terminal.get('error_message')), terminal)
        retained = self.config()
        self.run.check('hot_convergence_newer_config_retained', retained['metadata']['uid'] == newer['metadata']['uid']
                       and retained.get('data') == before.get('data')
                       and retained['metadata'].get('annotations', {}).get(key) == self.run.id,
                       {'before': env_projection(before), 'retained': env_projection(retained),
                        'original_condition': metadata_identity(before)})
        self.slot_released('hot_convergence_slot_and_lease_released')
        self.normal_hot(artifact_c, 'C', 'hot_convergence_fresh_request')
        self.join_holder(holder)
        observed = self.operation(holder[2])
        self.run.check('hot_convergence_failure_preserved', historical_terminal_preserved(terminal, observed),
                       {'before': terminal, 'after': observed, 'original_http': holder[1]})

    def join_holder(self, holder):
        thread, result, request_id = holder
        thread.join(timeout=15)
        if thread.is_alive():
            raise TimeoutError('Original hot HTTP observer did not finish after terminal recovery: ' + request_id)
        self.run.save('hot-original-http-' + request_id + '.json', result)

    def finish_threads(self):
        errors = []
        observations = []
        for thread, result, request_id in self.threads:
            association = self.associations.get(request_id) or {}
            started = association.get('request_started_monotonic')
            if not isinstance(started, (int, float)):
                raise AssertionError('Original hot HTTP observation deadline is missing')
            # Observe an already submitted request only within its original
            # deadline. Cleanup must not invent a shorter legal execution
            # window, nor give an unknown observer another full request budget.
            deadline = started + HOT_REQUEST_TIMEOUT_SECONDS
            observed_at = time.monotonic()
            remaining = max(0, deadline - observed_at)
            thread.join(timeout=remaining)
            alive = thread.is_alive()
            observations.append({'request_id': request_id,
                'operation_id': association.get('operation_id'),
                'original_started_monotonic': started,
                'original_absolute_deadline_monotonic': deadline,
                'remaining_seconds_before_wait': remaining,
                'waited_seconds': time.monotonic() - observed_at,
                'observer_still_alive': alive,
                'original_http_result': None if alive else dict(result)})
            self.run.save('hot-final-http-observers.json', observations)
            if alive:
                errors.append(request_id)
        if errors:
            raise TimeoutError('Owned hot HTTP observers exceeded their original request deadline: ' + str(errors))


def exercise(run):
    """Called after original Run.prepare/workspace; original finally still applies."""
    recovery = HotRecovery(run)
    artifacts = {}
    releases = {}
    artifacts['A'], releases['A'] = run.build('A')
    # Default workspace artifact retention is three. Consume A before later
    # builds can legitimately retire its static URL; keep its original ZIP.
    cold = run.api('/api/v1/userapp/' + run.app + '/start',
        {'user_id': run.user, 'request_id': 'hot-recovery-cold-' + run.id[:16], **artifacts['A']},
        base=run.entries[0], timeout=360)
    recovery.pod = run.prod_pod()
    run.check('hot_recovery_cold_A', run.content(run.id + '-A'), cold)
    recovery.seed_production_volume()
    for version in ('B', 'C', 'D'):
        artifacts[version], releases[version] = run.build(version)
    recovery.seed_data()
    image = runtime_fixture_image(recovery.pod)
    fixture = ArtifactFixture(run, image,
                              {version: (run.root / ('artifact-' + version + '.zip')).read_bytes()
                               for version in ('B', 'C', 'D')})
    failure = None
    cleanup_errors = []
    try:
        fixture.create()
        fault_artifacts = {version: {**artifacts[version], 'url': fixture.url(version)}
                           for version in ('B', 'C', 'D')}
        recovery.expired_lease_case(fixture, fault_artifacts['B'], releases['B'], fault_artifacts['C'])
        recovery.convergence_case(fixture, fault_artifacts['D'], releases['D'], fault_artifacts['C'])
        state = fixture.control('state')
        run.check('hot_recovery_no_artifact_replay', state['counts'] == {'B': 1, 'D': 1}
                  and state.get('static_counts') == {'C': 2}
                  and all(any(row.get('version') == version and row.get('event') == 'served'
                              for row in state['events']) for version in ('B', 'D')), state)
        run.check('hot_recovery_pod_preserved', recovery.physical_pod()['uid'] == recovery.pod['uid'],
                  {'before': recovery.pod, 'after': recovery.physical_pod()})
        recovery.data_preserved()
    except (Exception, KeyboardInterrupt) as error:
        failure = error
    finally:
        for version in ('B', 'D'):
            try:
                if fixture.current('pod') is not None:
                    fixture.control('release', version)
            except Exception as error:
                cleanup_errors.append('release ' + version + ': ' + repr(error))
        try:
            recovery.finish_threads()
        except Exception as error:
            cleanup_errors.append(repr(error))
        try:
            fixture.cleanup()
        except Exception as error:
            cleanup_errors.append(repr(error))
        run.summary['hot_recovery_original_error'] = repr(failure) if failure else None
        run.summary['hot_fixture_cleanup_errors'] = cleanup_errors
        run.save('summary.json', run.summary)
        run.save('hot-recovery-finally.json', {'original_error': repr(failure) if failure else None,
                                              'cleanup_errors': cleanup_errors})
    if cleanup_errors:
        raise RuntimeError('Hot recovery fixture cleanup failed: ' + repr(cleanup_errors)) from failure
    if failure:
        raise failure
