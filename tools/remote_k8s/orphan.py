"""Owned-namespace resource retirement acceptance. Real K8s/PG; no AI calls.

Only UUID resources created by this run are mutated. Dynamic PVCs and their data
are retained, including on failure. Raw SQL is limited to the fixture's own PG
and deliberately exercises canonical registrations which cannot be hydrated.
"""
import hashlib
import json
import re
import time
import uuid

from common import LABEL, atomic_json


CASE_REQUIRED = {
    'service_only': {'captured_uid', 'automatically_absent'},
    'stopped_controller': {'captured_uid', 'zero_replicas_no_pod', 'automatically_absent'},
    'desired_controller_without_pod': {'captured_uid', 'one_replica_no_pod', 'preserved_entire_window'},
    'pending_pod': {'captured_uid', 'pending', 'preserved_entire_window'},
    'running_pod': {'captured_uid', 'running', 'preserved_entire_window'},
    'foreign_service': {'captured_uid', 'preserved_entire_window'},
    'canonical_container_without_project': {'captured_uid', 'canonical_row', 'no_project_row', 'preserved_entire_window'},
    'canonical_missing_uid': {'captured_uid', 'canonical_row', 'uid_is_null', 'preserved_entire_window'},
    'canonical_bad_json': {'captured_uid', 'canonical_row', 'bad_json_retained', 'preserved_entire_window'},
    'builder_lifecycle_without_project': {'captured_uid', 'active_lifecycle', 'no_project_row', 'preserved_entire_window'},
    'retained_storage': {'cephfs_bound', 'rbd_bound', 'markers_written', 'pvc_uids_preserved', 'markers_after_retirement'},
    'cleanup': {'exact_owned_compute_gone', 'all_pvcs_retained', 'sql_fixture_rows_removed'},
}
OBSERVE_SECONDS = 12  # > fixture protection(2) + two scans + long-idle(4)


def literal(value):
    if not isinstance(value, str) or '\0' in value:
        raise ValueError('Invalid fixture SQL value')
    return "'" + value.replace("'", "''") + "'"


def require_profile(c, receipt):
    if c.get('PROFILE', 'full') != 'orphan' or receipt.get('fixture_profile') != 'orphan':
        raise ValueError('SUITE=orphan requires an explicitly deployed PROFILE=orphan fixture')
    if not re.fullmatch(r'rcoder-e2e-[a-z0-9][a-z0-9-]{0,35}', c.ns):
        raise ValueError('Orphan acceptance requires its dedicated owned namespace')


def deletion_path(kind, namespace, name):
    prefix, plural = {'Service': ('/api/v1', 'services'), 'Pod': ('/api/v1', 'pods'),
                      'StatefulSet': ('/apis/apps/v1', 'statefulsets'),
                      'ResourceQuota': ('/api/v1', 'resourcequotas')}[kind]
    return f'{prefix}/namespaces/{namespace}/{plural}/{name}'


def complete_cases(checks):
    return all(CASE_REQUIRED[name] <= {key for key, value in checks.get(name, {}).items() if value}
               for name in CASE_REQUIRED)


class Suite:
    def __init__(self, c, receipt, context, guard=lambda: None):
        require_profile(c, receipt)
        self.c, self.receipt, self.context, self.guard = c, receipt, context, guard
        self.run_id = uuid.uuid4().hex[:16]
        self.root = context['report_dir'] / 'orphan'
        self.root.mkdir(parents=True, exist_ok=False)
        self.deadline = time.monotonic() + 180
        self.checks, self.resources, self.claims, self.seeds = {}, [], {}, []
        self.intents, self.errors = [], []
        self.labels = {LABEL: c.id, 'rcoder.dev/orphan-run': self.run_id}
        namespace = self.get('Namespace', c.ns)
        if namespace['metadata'].get('labels', {}).get(LABEL) != c.id:
            raise RuntimeError('Refusing foreign namespace')
        self.namespace_uid = namespace['metadata']['uid']
        self.baseline_pvcs = {row['metadata']['name']: row['metadata']['uid'] for row in c.obj('get', 'pvc')['items']}
        postgres = self.get('Pod', 'postgres-0')
        if postgres['metadata'].get('labels', {}).get(LABEL) != c.id:
            raise RuntimeError('Refusing foreign PostgreSQL fixture')
        self.pg_uid = postgres['metadata']['uid']
        self.image = c.get('POSTGRES_IMAGE', 'postgres:16-alpine')
        deployment = self.get('Deployment', 'rcoder')
        if (deployment['metadata']['uid'] != receipt['deployment_uid']
                or deployment['metadata'].get('labels', {}).get(LABEL) != c.id
                or deployment['spec']['replicas'] != 2 or deployment.get('status', {}).get('readyReplicas') != 2):
            raise RuntimeError('Expected the recorded two-Ready-Main fixture')
        config_names = [volume['configMap']['name'] for volume in deployment['spec']['template']['spec']['volumes']
                        if volume['name'] == 'config' and 'configMap' in volume]
        if len(config_names) != 1:
            raise RuntimeError('Expected one actual mounted controller configuration')
        current_config = self.get('ConfigMap', config_names[0])
        if current_config['metadata'].get('labels', {}).get(LABEL) != c.id:
            raise RuntimeError('Refusing foreign controller configuration')
        self.config = json.loads(current_config['data']['config.yml'])
        settings = self.config.get('cleanup_config', {})
        if not (settings.get('orphan_reconcile_mode') == 'reclaim'
                and settings.get('container_protection_seconds') == 2
                and settings.get('long_idle_timeout_seconds') == 4
                and settings.get('idle_timeout_seconds') == 3600):
            raise RuntimeError('Unexpected orphan fixture cleanup policy')
        atomic_json(self.root / 'initial.json', {'run_id': self.run_id, 'namespace_uid': self.namespace_uid,
                    'postgres_uid': self.pg_uid, 'producer_source': receipt.get('source_sha256'),
                    'driver_source': context['snapshot_record']['source_sha256'], 'required': {k: sorted(v) for k, v in CASE_REQUIRED.items()}})

    def kube(self, *args, data=None, timeout=30):
        self.guard()
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Orphan acceptance deadline exceeded')
        return self.c.kube(*args, data=data, timeout=min(timeout, remaining))

    def get(self, kind, name):
        raw = self.kube('get', kind, name, '--ignore-not-found', '-o', 'json')
        return json.loads(raw) if raw.strip() else None

    def check(self, case, assertion, value, fatal=True):
        if assertion not in CASE_REQUIRED[case]:
            raise ValueError('Unregistered orphan assertion')
        self.checks.setdefault(case, {})[assertion] = bool(value)
        self.save()
        if not value and fatal:
            raise AssertionError(case + ': ' + assertion)

    def save(self):
        cases = [{'name': name, 'verdict': ('pass' if required <= {key for key, value in self.checks.get(name, {}).items() if value}
                  else 'fail' if name in self.checks else 'not_run'), 'required_assertions': sorted(required),
                  'assertions': self.checks.get(name, {})} for name, required in CASE_REQUIRED.items()]
        self.context['cases'] = cases
        atomic_json(self.root / 'summary.json', {'run_id': self.run_id, 'cases': cases, 'errors': self.errors,
                    'passed': not self.errors and complete_cases(self.checks), 'owned_resources': self.resources,
                    'retained_pvcs': self.claims, 'write_intents': self.intents})

    def create(self, row):
        kind, name = row['kind'], row['metadata']['name']
        if self.get(kind, name) is not None:
            raise RuntimeError('Refusing existing fixture name')
        row['metadata'].update(namespace=self.c.ns)
        row['metadata'].setdefault('labels', {}).update(self.labels)
        intent = {'kind': kind, 'name': name, 'action': 'create'}
        self.intents.append(intent)
        self.save()  # Unknown create is never blindly retried.
        current = json.loads(self.kube('create', '-f', '-', '-o', 'json', data=json.dumps(row)))
        captured = {'kind': kind, 'name': name, 'uid': current['metadata']['uid']}
        intent['uid'] = captured['uid']
        self.resources.append(captured)
        atomic_json(self.root / ('created-' + kind.lower() + '-' + name + '.json'), current)
        self.save()
        return current

    def exact_delete(self, captured):
        current = self.get(captured['kind'], captured['name'])
        if current is None:
            return
        if current['metadata']['uid'] != captured['uid'] or current['metadata'].get('labels', {}).get('rcoder.dev/orphan-run') != self.run_id:
            raise RuntimeError('Fixture cleanup identity changed')
        previous = next((intent for intent in self.intents if intent['action'] == 'delete'
                         and intent.get('uid') == captured['uid']), None)
        if previous is not None:
            # A lost acknowledgement is observed through the original UID;
            # it never causes another deletion request.
            self.wait(lambda: self.get(captured['kind'], captured['name']) is None)
            return
        intent = {**captured, 'action': 'delete'}
        self.intents.append(intent)
        self.save()
        options = {'apiVersion': 'v1', 'kind': 'DeleteOptions', 'preconditions': {'uid': captured['uid']},
                   'propagationPolicy': 'Foreground'}
        self.kube('delete', '--raw', deletion_path(captured['kind'], self.c.ns, captured['name']), '-f', '-', data=json.dumps(options))
        intent['acknowledged'] = True
        self.wait(lambda: self.get(captured['kind'], captured['name']) is None)

    def wait(self, action, budget=60):
        deadline = min(self.deadline, time.monotonic() + budget)
        while True:
            value = action()
            if value:
                return value
            if time.monotonic() >= deadline:
                raise TimeoutError('Orphan fixture observation timed out')
            time.sleep(.4)

    def sql(self, query):
        pod = self.get('Pod', 'postgres-0')
        if pod is None or pod['metadata']['uid'] != self.pg_uid:
            raise RuntimeError('Fixture PostgreSQL incarnation changed')
        return self.kube('exec', 'postgres-0', '--', 'psql', '-X', '-v', 'ON_ERROR_STOP=1', '-U', 'rcoder', '-d', 'rcoder', '-Atc', query).strip()

    def key(self, case, family='computer-agent-runner'):
        identifier = 'o' + self.run_id + hashlib.sha256(case.encode()).hexdigest()[:8]
        service = self.config['kubernetes_config']['services'][family]
        prefix = service.get('image_tag_prefix') or {'computer-agent-runner': 'computer-agent-runner', 'user-app-builder': 'rcoder-app-builder'}[family]
        name = prefix.replace('_', '-') + '-' + identifier
        labels = {'app.kubernetes.io/managed-by': 'rcoder-runtime', 'app.kubernetes.io/instance': identifier,
                  'rcoder.io/service-type': family, 'rcoder.io/identifier': identifier}
        return identifier, name, labels

    def service(self, case, family='computer-agent-runner', foreign=False):
        identifier, name, labels = self.key(case, family)
        if foreign:
            labels['app.kubernetes.io/managed-by'] = 'foreign-fixture'
        row = self.create({'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': name + '-svc', 'labels': labels},
                           'spec': {'selector': labels, 'ports': [{'name': 'grpc', 'port': 50051}]}})
        self.check(case, 'captured_uid', bool(row['metadata']['uid']))
        return identifier, name, row

    def seed_registration(self, case, missing_uid=False, bad_json=False):
        identifier, name, _ = self.key(case)
        generation = uuid.uuid4().hex
        now = int(time.time() * 1000000)
        self.seeds.append({'kind': 'registration', 'name': name, 'id': identifier, 'generation': generation})
        self.save()
        columns = 'container_name,container_generation,container_id,logical_id,service_type,container_ip,internal_port,external_port,status,service_url,last_activity_at_us,created_at_us,row_revision'
        values = [literal(name), literal(generation), 'NULL' if missing_uid else literal('fixture-' + generation), literal(identifier), literal('computer-agent-runner'), literal('127.0.0.1'), '8086', '0', literal('stopped'), literal('http://fixture'), str(now), str(now), '1']
        sql = 'BEGIN; INSERT INTO containers(' + columns + ') VALUES(' + ','.join(values) + ');'
        if missing_uid or bad_json:
            sql += 'INSERT INTO projects(project_id,generation,container_name,container_generation,service_type,payload_version,last_activity_at_us,created_at_us,row_revision,model_provider_json) VALUES(' + ','.join([literal(identifier), literal(generation), literal(name), literal(generation), literal('computer-agent-runner'), '1', str(now), str(now), '1', literal('{invalid-json') if bad_json else 'NULL']) + ');'
        self.sql(sql + 'COMMIT;')
        self.check(case, 'canonical_row', self.sql('SELECT count(*) FROM containers WHERE container_name=' + literal(name)) == '1')
        if missing_uid:
            self.check(case, 'uid_is_null', self.sql('SELECT container_id IS NULL FROM containers WHERE container_name=' + literal(name)) == 't')
        elif bad_json:
            self.check(case, 'bad_json_retained', self.sql('SELECT model_provider_json FROM projects WHERE project_id=' + literal(identifier)) == '{invalid-json')
        else:
            self.check(case, 'no_project_row', self.sql('SELECT count(*) FROM projects WHERE container_name=' + literal(name)) == '0')

    def markers(self, verify=False):
        for claim in self.claims.values():
            current = self.get('PersistentVolumeClaim', claim['name'])
            if current is None or current['metadata']['uid'] != claim['uid'] or current.get('status', {}).get('phase') != 'Bound':
                raise RuntimeError('Refusing changed or unbound marker PVC')
        name = 'orphan-markers-' + self.run_id + ('-verify' if verify else '')
        marker = 'owned-' + self.run_id
        script = 'set -eu; ' + ('test "$(cat /ceph/marker)" = "$1"; test "$(cat /rbd/marker)" = "$1"; ' if verify else 'printf %s "$1" > /ceph/marker; printf %s "$1" > /rbd/marker; ') + 'exec sleep 3600'
        row = self.create({'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name}, 'spec': {
            'restartPolicy': 'Never', 'securityContext': {'runAsUser': 0, 'fsGroup': 0}, 'containers': [{'name': 'marker', 'image': self.image,
                'command': ['sh', '-ec', script, '--', marker], 'volumeMounts': [{'name': 'ceph', 'mountPath': '/ceph'}, {'name': 'rbd', 'mountPath': '/rbd'}]}],
            'volumes': [{'name': role, 'persistentVolumeClaim': {'claimName': claim['name']}} for role, claim in self.claims.items()]}})
        captured = self.resources[-1]
        self.wait(lambda: self.get('Pod', name).get('status', {}).get('phase') == 'Running')
        output = self.wait(lambda: self.kube('exec', name, '--', 'sh', '-ec',
            'if test -f /ceph/marker && test -f /rbd/marker; then cat /ceph/marker; printf "\\n"; cat /rbd/marker; fi'))
        self.check('retained_storage', 'markers_after_retirement' if verify else 'markers_written', output.splitlines() == [marker, marker])
        self.exact_delete(captured)

    def prepare(self):
        for role, sc, access in [('ceph', self.c.get('STORAGE_CLASS', 'cephfs'), 'ReadWriteMany'),
                                 ('rbd', self.c.get('PG_STORAGE_CLASS', 'ceph-rbd'), 'ReadWriteOnce')]:
            name = 'orphan-' + role + '-' + self.run_id
            row = self.create({'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim', 'metadata': {'name': name},
                               'spec': {'storageClassName': sc, 'accessModes': [access], 'resources': {'requests': {'storage': '1Gi'}}}})
            self.claims[role] = {'name': name, 'uid': row['metadata']['uid']}
            self.wait(lambda: self.get('PersistentVolumeClaim', name).get('status', {}).get('phase') == 'Bound')
            self.check('retained_storage', 'cephfs_bound' if role == 'ceph' else 'rbd_bound', True)
        self.markers()

    def controller(self, case, replicas, pending=False):
        identifier, name, labels = self.key(case)
        template = {'metadata': {'labels': labels}, 'spec': {'containers': [{'name': 'agent', 'image': self.image, 'command': ['sh', '-ec', 'exec sleep 3600']}],
                    'volumes': [{'name': role, 'persistentVolumeClaim': {'claimName': claim['name']}} for role, claim in self.claims.items()]}}
        if pending:
            template['spec']['nodeSelector'] = {'rcoder.dev/nonexistent-node': self.run_id}
        row = self.create({'apiVersion': 'apps/v1', 'kind': 'StatefulSet', 'metadata': {'name': name, 'labels': labels},
                           'spec': {'replicas': replicas, 'serviceName': name + '-headless', 'selector': {'matchLabels': labels}, 'template': template}})
        self.check(case, 'captured_uid', bool(row['metadata']['uid']))
        return row

    def execute(self):
        self.prepare()
        protected = []
        for case, missing, malformed in [('canonical_container_without_project', False, False), ('canonical_missing_uid', True, False), ('canonical_bad_json', False, True)]:
            self.seed_registration(case, missing, malformed)
            protected.append((case, self.service(case)[2]))
        case = 'builder_lifecycle_without_project'
        identifier, _, _ = self.key(case, 'user-app-builder')
        lifecycle = uuid.uuid4().hex
        self.seeds.append({'kind': 'lifecycle', 'id': identifier, 'generation': lifecycle}); self.save()
        now = int(time.time() * 1000000)
        self.sql('INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us) VALUES(' + ','.join([literal(identifier), literal(lifecycle), '1', literal('active'), '1', str(now), str(now)]) + ')')
        self.check(case, 'active_lifecycle', self.sql('SELECT lifecycle_state FROM userapps WHERE app_id=' + literal(identifier)) == 'active')
        self.check(case, 'no_project_row', self.sql('SELECT count(*) FROM projects WHERE project_id=' + literal(identifier)) == '0')
        protected.append((case, self.service(case, 'user-app-builder')[2]))
        protected.append(('foreign_service', self.service('foreign_service', foreign=True)[2]))
        for case, pending in [('pending_pod', True), ('running_pod', False)]:
            _, name, labels = self.key(case)
            spec = {'containers': [{'name': 'agent', 'image': self.image, 'command': ['sh', '-ec', 'exec sleep 3600']}], 'restartPolicy': 'Never'}
            if pending:
                spec['nodeSelector'] = {'rcoder.dev/nonexistent-node': self.run_id}
            row = self.create({'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name, 'labels': labels}, 'spec': spec})
            self.check(case, 'captured_uid', bool(row['metadata']['uid']))
            phase = 'Pending' if pending else 'Running'
            self.wait(lambda: self.get('Pod', name).get('status', {}).get('phase') == phase)
            self.check(case, 'pending' if pending else 'running', True)
            protected.append((case, row))
        # Saturate only this dedicated namespace's Pod count. Existing Main/PG
        # keep running; a new desired STS has genuinely no Pod, rather than
        # silently substituting a Pending Pod for the controller-only case.
        count = len(self.c.obj('get', 'pods')['items'])
        quota = self.create({'apiVersion': 'v1', 'kind': 'ResourceQuota', 'metadata': {'name': 'orphan-quota-' + self.run_id}, 'spec': {'hard': {'count/pods': str(count)}}})
        self.wait(lambda: self.get('ResourceQuota', quota['metadata']['name']).get('status', {}).get('used', {}).get('count/pods') == str(count))
        active = self.controller('desired_controller_without_pod', 1)
        self.check('desired_controller_without_pod', 'one_replica_no_pod', active['spec']['replicas'] == 1 and self.get('Pod', active['metadata']['name'] + '-0') is None)
        protected.append(('desired_controller_without_pod', active))
        service = self.service('service_only')[2]
        stopped = self.controller('stopped_controller', 0)
        self.check('stopped_controller', 'zero_replicas_no_pod', stopped['spec']['replicas'] == 0 and self.get('Pod', stopped['metadata']['name'] + '-0') is None)
        observe_until = time.monotonic() + OBSERVE_SECONDS
        preserved = {case for case, _ in protected}
        while time.monotonic() < observe_until:
            for case, original in protected:
                current = self.get(original['kind'], original['metadata']['name'])
                if current is None or current['metadata']['uid'] != original['metadata']['uid']:
                    preserved.discard(case)
                if case == 'desired_controller_without_pod' and self.get('Pod', original['metadata']['name'] + '-0') is not None:
                    self.check(case, 'one_replica_no_pod', False, fatal=False)
            time.sleep(.4)
        for case, _ in protected:
            self.check(case, 'preserved_entire_window', case in preserved, fatal=False)
        positive = [('service_only', service), ('stopped_controller', stopped)]
        reclaimed = set()
        positive_deadline = min(self.deadline, time.monotonic() + 60)
        while time.monotonic() < positive_deadline:
            for case, original in positive:
                if self.get(original['kind'], original['metadata']['name']) is None:
                    reclaimed.add(case)
            if len(reclaimed) == len(positive):
                break
            time.sleep(.4)
        for case, _ in positive:
            self.check(case, 'automatically_absent', case in reclaimed, fatal=False)
        self.check('retained_storage', 'pvc_uids_preserved', all(self.get('PersistentVolumeClaim', claim['name'])['metadata']['uid'] == claim['uid'] for claim in self.claims.values()))

    def cleanup(self):
        self.deadline = time.monotonic() + 120
        failures = []
        for resource in reversed(self.resources):
            if resource['kind'] == 'PersistentVolumeClaim':
                continue
            try:
                self.exact_delete(resource)
            except Exception as error:
                failures.append(type(error).__name__ + ': ' + str(error))
        unconfirmed = [intent for intent in self.intents if intent['action'] == 'create' and not intent.get('uid')]
        self.checks.setdefault('cleanup', {})['exact_owned_compute_gone'] = not failures and not unconfirmed
        self.errors.extend('cleanup: ' + error for error in failures)
        if unconfirmed:
            self.errors.append('cleanup: unconfirmed create intents retained for exact identity observation')
        if 'pvc_uids_preserved' in self.checks.get('retained_storage', {}):
            try:
                self.markers(verify=True)
            except Exception as error:
                self.errors.append('marker verification: ' + type(error).__name__ + ': ' + str(error))
        expected_claims = {**self.baseline_pvcs, **{claim['name']: claim['uid'] for claim in self.claims.values()}}
        self.checks['cleanup']['all_pvcs_retained'] = all(self.get('PersistentVolumeClaim', name)['metadata']['uid'] == uid for name, uid in expected_claims.items())
        for seed in self.seeds:
            try:
                if seed['kind'] == 'registration':
                    self.sql('BEGIN; DELETE FROM projects WHERE project_id=' + literal(seed['id']) + ' AND generation=' + literal(seed['generation']) + '; DELETE FROM containers WHERE container_name=' + literal(seed['name']) + ' AND container_generation=' + literal(seed['generation']) + '; COMMIT;')
                else:
                    self.sql('DELETE FROM userapps WHERE app_id=' + literal(seed['id']) + ' AND lifecycle_id=' + literal(seed['generation']))
            except Exception as error:
                self.errors.append('SQL cleanup: ' + type(error).__name__ + ': ' + str(error))
        self.checks['cleanup']['sql_fixture_rows_removed'] = all(self.sql('SELECT count(*) FROM ' + ('containers WHERE container_name=' + literal(seed['name']) if seed['kind'] == 'registration' else 'userapps WHERE app_id=' + literal(seed['id']))) == '0' for seed in self.seeds)


def run(c, receipt, context, guard=lambda: None):
    suite = Suite(c, receipt, context, guard)
    try:
        suite.execute()
    except BaseException as error:
        suite.errors.append(type(error).__name__ + ': ' + str(error))
        raise
    finally:
        try:
            suite.cleanup()
        except Exception as error:
            suite.errors.append('cleanup: ' + type(error).__name__ + ': ' + str(error))
        suite.save()
    if suite.errors or not complete_cases(suite.checks):
        raise AssertionError('Orphan acceptance has failed or missing required assertions')
    return context['cases']
