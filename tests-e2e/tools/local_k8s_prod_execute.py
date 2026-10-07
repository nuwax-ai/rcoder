#!/usr/bin/env python3
"""Real owned local K8s cold/hot/Lease/R2/R1 execution; no Cargo, AI or purge.

Requires a passed Stage1 build receipt, two frozen host processes and an owned
artifact server. Failure retains all resources and original identities. Success
only submits compute Stop through RCoder; every PVC/namespace is retained.
"""
import argparse
import base64
import datetime
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import re
import subprocess
import threading
import time
import urllib.parse
import uuid

from local_k8s_core import Harness as CoreHarness, digest, require, safe, validate_inputs, resource_identity, source_snapshot
from local_k8s_prod import artifact_proposal, ARTIFACT_SERVER, validate_busy

RUNTIME_IMAGE = 'nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/app-runtime@sha256:eddbe306fac7a65c155adfd7f264e0e14a0acb2755002df7f6308d828fda0831'


def current_template(value):
    # Match the typed Rust view: omitted optional fields and JSON null both
    # deserialize as None. Preserve lists and all meaningful template fields.
    def optional(item):
        if isinstance(item,dict):return {key:optional(val) for key,val in item.items() if val is not None}
        if isinstance(item,list):return [optional(val) for val in item]
        return item
    result=optional(value)
    result.get('metadata',{}).get('labels',{}).pop('pod-template-hash',None)
    return result


def selector_matches(labels, selector):
    if not all(labels.get(key)==value for key,value in selector.get('matchLabels',{}).items()):return False
    for expression in selector.get('matchExpressions',[]):
        key,op,values=expression['key'],expression['operator'],expression.get('values',[])
        if op=='In':match=key in labels and labels[key] in values
        elif op=='NotIn':match=key not in labels or labels[key] not in values
        elif op=='Exists':match=key in labels
        elif op=='DoesNotExist':match=key not in labels
        else:raise RuntimeError('unsupported actual Deployment selector operator')
        if not match:return False
    return True


def current_prod_pod(deployment, pods, replicasets, namespace, app):
    dep_meta=deployment['metadata'];template=current_template(deployment['spec']['template'])
    selector=deployment['spec']['selector'];rs_by_name={row['metadata']['name']:row for row in replicasets}
    token=template.get('metadata',{}).get('annotations',{}).get('rcoder.io/deploy-template-token')
    candidates=[]
    for pod in pods:
        meta=pod['metadata'];labels=meta.get('labels',{})
        if meta.get('namespace')!=namespace or not meta.get('uid') or meta.get('deletionTimestamp'):
            continue
        if labels.get('app.kubernetes.io/managed-by')!='rcoder-app-manager' or labels.get('rcoder.io/app-id')!=app or not selector_matches(labels,selector):
            continue
        if meta.get('annotations',{}).get('rcoder.io/deploy-template-token')!=token:continue
        owner=next((row for row in meta.get('ownerReferences',[]) if row.get('controller') is True),None)
        if not owner or owner.get('apiVersion')!='apps/v1' or owner.get('kind')!='ReplicaSet':continue
        rs=rs_by_name.get(owner.get('name'));rm=rs.get('metadata',{}) if rs else {}
        if not rs or rm.get('namespace')!=namespace or rm.get('uid')!=owner.get('uid') or rm.get('deletionTimestamp'):continue
        if not any(row.get('controller') is True and row.get('apiVersion')=='apps/v1' and row.get('kind')=='Deployment'
                   and row.get('name')==dep_meta['name'] and row.get('uid')==dep_meta['uid'] for row in rm.get('ownerReferences',[])):
            continue
        if current_template(rs.get('spec',{}).get('template',{}))!=template:continue
        if not any(container.get('name')=='app' for container in pod.get('spec',{}).get('containers',[])):continue
        candidates.append((pod,rs))
    require(len(candidates)==1,'exactly one non-terminating current-template Prod Pod is required')
    return candidates[0]


def stage1_frozen_proof(report):
    checks=[row for row in report.get('checks',[]) if row.get('name')=='build source host binary and harness remain frozen']
    require(len(checks)==1 and checks[0].get('passed') is True,'Stage1 lacks the actual passed final input-freeze check')
    evidence=checks[0].get('evidence',{})
    before,after=evidence.get('before'),evidence.get('after')
    require(isinstance(before,dict) and before==after and before==report.get('inputs_before'),'Stage1 actual before/after proof disagrees with its captured initial input')
    if 'inputs_after' in report:require(report['inputs_after']==after,'Stage1 explicit final inputs disagree with actual passed check')
    source=after.get('source',{}).get('source_inputs_sha256')
    require(isinstance(source,str) and re.fullmatch(r'[0-9a-f]{64}',source),'Stage1 actual proof lacks source identity')
    return after


def owned_receipt(root, reference, label, json_value=True):
    require(isinstance(reference, dict), label + ' must be an exact retained receipt reference')
    path = Path(reference.get('path', '')).resolve()
    require(path.is_file() and path.is_relative_to(root), label + ' escapes the owned run or is absent')
    expected = reference.get('sha256', '')
    require(isinstance(expected, str) and re.fullmatch(r'[0-9a-f]{64}', expected)
            and digest(path) == expected, label + ' bytes changed')
    return json.loads(path.read_text()) if json_value else path


def validate_controller_revision(proof, stage1, current, original_report_sha256, app, namespace, run_id, lifecycle):
    require(proof.get('schema_version') == 1 and proof.get('app_id') == app and proof.get('namespace') == namespace
            and proof.get('run_id') == run_id and proof.get('lifecycle_id') == lifecycle, 'controller revision scope differs')
    require(proof.get('stage1_report', {}).get('sha256') == original_report_sha256,
            'controller revision did not authorize this exact historical artifact receipt')
    historical = stage1['source']['source_inputs_sha256']
    observed = current['source']['source_inputs_sha256']
    require(proof.get('historical_source_inputs_sha256') == historical
            and proof.get('controller_source_inputs_sha256') == observed,
            'historical artifact or current controller source identity differs')
    require(current.get('source_bound_to_build') is True, 'current controller is not bound to an actual build')


def validate_stopped_prod(proof, snapshot, app, namespace, lifecycle, namespace_uid):
    require(proof.get('schema_version') == 1 and proof.get('app_id') == app and proof.get('namespace') == namespace
            and proof.get('lifecycle_id') == lifecycle and proof.get('namespace_uid') == namespace_uid,
            'stopped Prod receipt scope or namespace UID differs')
    original = proof.get('original_failed_operation', {})
    require(original.get('app_id') == app and original.get('lifecycle_id') == lifecycle
            and original.get('scope') == 'Prod' and original.get('kind') == 'StartDeployment'
            and original.get('state') == 'Failed' and original.get('operation_id') and original.get('request_id'),
            'original failed cold operation must remain a real Failed record')
    receipt, terminal = proof.get('compute_stop_receipt', {}), proof.get('compute_stop_terminal', {})
    require(receipt.get('app_id') == app and receipt.get('lifecycle_id') == lifecycle
            and receipt.get('scope') == 'Prod' and receipt.get('action') == 'stop'
            and receipt.get('operation_id') and receipt.get('operation_id') != original['operation_id']
            and receipt.get('status_url') == '/computer/pod/operations/' + app + '/' + receipt['operation_id'],
            'original Prod compute Stop receipt differs')
    require(all(terminal.get(key) == receipt.get(key) for key in ('operation_id', 'app_id', 'lifecycle_id', 'scope', 'action'))
            and terminal.get('state') == 'succeeded' and terminal.get('stage') == 'completed',
            'compute Stop has not confirmed its own completed terminal result')
    previous = proof.get('stopped_physical', {})
    require(previous.get('pod') is None and snapshot.get('pod') is None
            and previous.get('deployment', {}).get('uid') and previous['deployment']['uid'] == snapshot['deployment']['uid']
            and previous.get('pvc', {}).get('uid') and previous['pvc']['uid'] == snapshot['pvc']['uid']
            and previous['pvc'].get('volume_name') and previous['pvc']['volume_name'] == snapshot['pvc']['volume_name'],
            'captured stopped Prod Deployment/PVC/PV was replaced')
    return original, receipt


def operation_identity(record, app, lifecycle, request_id, kind=None):
    require(isinstance(record, dict) and record.get('app_id') == app
            and record.get('lifecycle_id') == lifecycle and record.get('request_id') == request_id
            and record.get('scope') == 'Prod' and record.get('operation_id'), 'original durable Prod request identity differs')
    if kind:require(record.get('kind') == kind, 'original durable operation kind differs')
    require(record.get('state') in ('Pending', 'Running', 'Succeeded'), 'original Prod operation failed or needs recovery')
    return record


def compact_lease(row, namespace, app):
    metadata, spec = row.get('metadata', {}), row.get('spec', {})
    require(metadata.get('namespace') == namespace and metadata.get('name') == 'rcoder-operation-prod-' + app
            and metadata.get('uid') and metadata.get('resourceVersion'), 'real Lease identity missing or foreign')
    labels = metadata.get('labels', {})
    require(labels.get('rcoder.io/operation-app') == app and labels.get('rcoder.io/operation-family') == 'user-app', 'Lease family/application differs')
    require(spec.get('holderIdentity') and spec.get('renewTime') and spec.get('leaseDurationSeconds', 0) > 0, 'Lease has no live ownership evidence')
    renewed = datetime.datetime.fromisoformat(spec['renewTime'].replace('Z', '+00:00'))
    require(renewed.tzinfo is not None and datetime.datetime.now(datetime.timezone.utc) < renewed + datetime.timedelta(seconds=spec['leaseDurationSeconds']), 'original Lease is expired, not a live holder')
    # Hash the physical holder token in exported evidence; never confuse it with
    # the separately verified durable operation ID or disclose a private token.
    return {**resource_identity(row), 'holder_sha256': hashlib.sha256(spec['holderIdentity'].encode()).hexdigest(),
            'renew_time': spec['renewTime'], 'duration_seconds': spec['leaseDurationSeconds']}


class Execute(CoreHarness):
    def __init__(self, root, primary, secondary, app, budget=1800, build_report=None, report_suffix=None, controller_revision_proof=None, stopped_prod_proof=None):
        self.root, self.identity, self.kubeconfig, self.url = validate_inputs(root, primary, app)
        _, _, _, secondary = validate_inputs(root, secondary, app)
        require(secondary != self.url, 'cross-controller test requires independent URLs/processes')
        self.secondary = secondary
        self.app, self.namespace = app, self.identity['namespace']
        self.deadline, self.namespace_uid = time.monotonic() + budget, None
        self.observation_clock=threading.local()
        self.lock = threading.Lock()
        suffix=report_suffix or uuid.uuid4().hex[:12]
        require(re.fullmatch(r'[a-z0-9-]{1,40}',suffix),'execution report suffix must be a literal owned identifier')
        self.report_path = self.root / (getattr(self,'report_prefix','local-k8s-prod-execute-') + app + '-' + suffix + '.json')
        require(not self.report_path.exists(), 'never overwrite original execution evidence')
        self.report = {'success': False, 'scope': 'real local K8s cold/hot/Lease/R2/R1 release and exhausted budget',
            'app_id': app, 'namespace': self.namespace, 'run_id': self.identity['run_id'], 'checks': [], 'timeline': [],
            'pending_requests': [], 'original_operations': [], 'policy': 'no PVC/namespace/purge/deletion; failure preserves live scene',
            'not_covered': ['traffic wake-specific holder', 'actual HTTP disconnect', 'unknown apiserver write/cleanup response faults']}
        build_path = Path(build_report).resolve() if build_report else self.root / ('local-k8s-prod-' + app + '.json')
        require(build_path.is_file() and build_path.is_relative_to(self.root), 'original Stage1 receipt must be retained inside this owned run')
        self.build = json.loads(build_path.read_text())
        require(self.build.get('success') is True and self.build.get('stage') == 'build_ready'
                and self.build.get('app_id') == app and self.build.get('namespace') == self.namespace
                and self.build.get('run_id') == self.identity['run_id'], 'passed original Stage1 build receipt required')
        self.stage1_inputs=stage1_frozen_proof(self.build)
        self.report['stage1_input_proof']={'proof_source':'actual passed Stage1 final check','original_report':str(build_path),'original_report_sha256':digest(build_path),'snapshot':self.stage1_inputs}
        self.lifecycle_id = self.build['lifecycle_id']
        self.revision = None
        if controller_revision_proof is not None:
            proof_path = Path(controller_revision_proof).resolve()
            require(proof_path.is_file() and proof_path.is_relative_to(self.root), 'controller revision receipt must be retained inside this owned run')
            self.revision = json.loads(proof_path.read_text())
            retained_stage1 = owned_receipt(self.root, self.revision.get('stage1_report'), 'historical Stage1 report')
            require(retained_stage1 == self.build, 'controller revision names another Stage1 artifact receipt')
            self.report['controller_revision_proof'] = {'path':str(proof_path),'sha256':digest(proof_path)}
        self.stopped_proof = None
        if stopped_prod_proof is not None:
            require(self.revision is not None, 'stopped prior Prod resume requires an explicit controller revision proof')
            stopped_path = Path(stopped_prod_proof).resolve()
            require(stopped_path.is_file() and stopped_path.is_relative_to(self.root), 'stopped Prod receipt must be retained inside this owned run')
            self.stopped_proof = json.loads(stopped_path.read_text())
            self.report['stopped_prod_proof'] = {'path':str(stopped_path),'sha256':digest(stopped_path)}
        self.sentinel_name = '.local-k8s-prod-sentinel-' + suffix

        self.original = self.build['builder']
        self.artifacts = self.build['artifacts']
        for version in ('A', 'B'):
            path = Path(self.artifacts[version]['path']).resolve()
            require(path.is_relative_to(self.root) and path.is_file() and digest(path) == self.artifacts[version]['sha256'], 'original Stage1 artifact bytes changed')
        self.artifact_name = 'core-artifacts-' + app
        self.artifact_uid = None
        self.prod_original = None
        self.persist()

    def input_proof(self):
        if self.revision is None:
            return CoreHarness.input_proof(self)
        build = owned_receipt(self.root,self.revision.get('build_receipt'),'current successful Cargo build')
        primary = owned_receipt(self.root,self.revision.get('primary_process_receipt'),'current primary host process')
        repo = Path(__file__).resolve().parents[2]
        source = source_snapshot(repo)
        require(source['source_inputs_sha256'] == build.get('source_inputs_sha256'), 'current source differs from actual controller build')
        binary = Path(build.get('frozen_binary','')).resolve()
        require(type(build.get('cargo_exit')) is int and build['cargo_exit'] == 0 and binary.is_file()
                and binary.is_relative_to(self.root) and digest(binary) == build.get('sha256')
                and binary.stat().st_mode & 0o222 == 0, 'current controller is not a frozen successful owned build')
        require(primary.get('namespace') == self.namespace and primary.get('url','').rstrip('/') == self.url
                and Path(primary.get('executable','')).resolve() == binary
                and type(primary.get('pid')) is int and primary['pid'] > 0, 'current primary process receipt differs')
        observed = subprocess.run(['ps','-p',str(primary['pid']),'-o','lstart=','-o','comm='],capture_output=True,text=True,timeout=self.remaining(10))
        require(observed.returncode == 0 and str(binary) in observed.stdout, 'current primary frozen process is absent')
        return {'source':{key:source[key] for key in ['commit','source_inputs_sha256','diff_sha256']},'binary':str(binary),
                'binary_sha256':digest(binary),'source_bound_to_build':True,'host_pid':primary['pid'],
                'host_process_start_and_executable':observed.stdout.strip()}

    def controller_proof(self):
        require(digest(self.report['stage1_input_proof']['original_report']) == self.report['stage1_input_proof']['original_report_sha256'], 'original Stage1 report bytes changed')
        for artifact in self.artifacts.values():
            require(digest(artifact['path']) == artifact['sha256'] and Path(artifact['path']).stat().st_mode & 0o222 == 0, 'historical artifact is not the frozen original byte stream')
        if self.revision is not None:
            ref = self.report['controller_revision_proof']
            require(digest(ref['path']) == ref['sha256'], 'controller revision receipt bytes changed')
        primary = self.input_proof()
        if self.revision is None:
            process = json.loads((self.root / 'host-secondary-process.json').read_text())
            require(primary['source']['source_inputs_sha256'] == self.stage1_inputs['source']['source_inputs_sha256'], 'Stage1 and execution production source differ')
        else:
            validate_controller_revision(self.revision,self.stage1_inputs,primary,self.report['stage1_input_proof']['original_report_sha256'],
                                         self.app,self.namespace,self.identity['run_id'],self.lifecycle_id)
            require(self.revision.get('namespace_uid') == self.namespace_uid, 'controller revision namespace was replaced')
            process = owned_receipt(self.root,self.revision.get('secondary_process_receipt'),'current secondary host process')
            # Hash retained actual command output, then inspect its recorded exit.
            # A receipt alone cannot turn a failed regression into a pass.
            for name in ('pod_selection','project_identity'):
                regression = self.revision.get('regressions',{}).get(name,{})
                for phase in ('before','after'):
                    evidence = regression.get(phase,{})
                    owned_receipt(self.root,evidence.get('log'),name+' '+phase+' command log',False)
                    exit_path = owned_receipt(self.root,evidence.get('exit'),name+' '+phase+' actual exit',False)
                    exit_code = int(exit_path.read_text().strip())
                    require(exit_code != 0 if phase=='before' else exit_code==0, name+' '+phase+' regression evidence has the wrong exit')
                require(regression.get('after',{}).get('source_inputs_sha256') == primary['source']['source_inputs_sha256'], name+' after evidence is not bound to current source')
            live_before = owned_receipt(self.root,self.revision.get('live_failure_before'),'retained real wrong-Pod failure')
            require(live_before.get('app') == self.app and live_before.get('original_operation_id'), 'real wrong-Pod before belongs to another application')
            if self.stopped_proof is not None:
                require(live_before['original_operation_id'] == self.stopped_proof.get('original_failed_operation',{}).get('operation_id'), 'stopped resume refers to another failed cold operation')
        require(process.get('namespace') == self.namespace and process.get('url', '').rstrip('/') == self.secondary
                and type(process.get('pid')) is int and process['pid'] > 0 and process['pid'] != primary['host_pid']
                and Path(process.get('executable', '')).resolve() == Path(primary['binary']), 'second controller must run the same frozen binary independently')
        observed = subprocess.run(['ps','-p',str(process['pid']),'-o','lstart=','-o','comm='],capture_output=True,text=True,timeout=self.remaining(10))
        require(observed.returncode == 0 and primary['binary'] in observed.stdout, 'second frozen controller process absent')
        return {'primary':primary,'secondary_pid':process['pid'],'secondary_process':observed.stdout.strip(),
                'historical_artifact_source_inputs_sha256':self.stage1_inputs['source']['source_inputs_sha256'],
                'controller_revision_receipt':self.report.get('controller_revision_proof'),
                'build_harness_sha256':digest(Path(__file__).with_name('local_k8s_prod.py')),
                'core_harness_sha256':digest(Path(__file__).with_name('local_k8s_core.py')),'execute_harness_sha256':digest(__file__)}

    def call(self, method, route, body=None, secondary=False, cap=300, raw=False):
        require(route.startswith('/') and not route.startswith('//'), 'relative original API route required')
        if method != 'GET':self.ownership()
        endpoint = self.secondary if secondary else self.url
        data = json.dumps(body).encode() if body is not None else None
        pending = {'method': method, 'route': route, 'controller': endpoint, 'request': safe(body), 'result_unknown': method != 'GET'}
        if method != 'GET':self.report['pending_requests'].append(pending);self.event('dispatch', pending)
        timeout = self.remaining(cap)
        command = ['curl', '--silent', '--show-error', '--noproxy', '*', '--max-time', str(timeout), '--request', method,
                   '--header', 'Content-Type: application/json', '--header', 'Accept-Language: en-US', '--header', 'x-app-id: ' + self.app,
                   '--write-out', '\n%{http_code}']
        if data is not None:command += ['--data-binary', '@-']
        result = subprocess.run([*command, endpoint + route], input=data, capture_output=True, timeout=timeout + 1)
        if result.returncode:
            self.event('response_unconfirmed', {**pending, 'exit': result.returncode, 'cause': safe(result.stderr.decode(errors='replace'))})
            raise RuntimeError('original HTTP transfer unconfirmed; do not replay a dispatched write')
        content, status = result.stdout.rsplit(b'\n', 1)
        require(status.isdigit(), 'actual HTTP status absent')
        status = int(status)
        value = content if raw else json.loads(content)
        if method != 'GET':pending.update(result_unknown=False, http_status=status)
        self.event('http', {'method': method, 'route': route, 'controller': endpoint, 'status': status,
            'value': {'bytes': len(content), 'sha256': hashlib.sha256(content).hexdigest()} if raw else value})
        return status, value

    def api(self, route, body=None, secondary=False, cap=300):
        status, value = self.call('POST' if body is not None else 'GET', route, body, secondary, cap)
        require(status == 200 and value.get('code') == '0000' and value.get('success') is True, 'real API operation failed: ' + str(safe(value)))
        return value['data']

    def object(self, kind, name):
        raw = self.kube(['get', kind, name, '--ignore-not-found=true', '-o', 'json'], json_output=False)
        return json.loads(raw) if raw else None

    def validate_artifact_resource(self, row, kind):
        metadata = row.get('metadata', {})
        require(row.get('kind') == kind and metadata.get('namespace') == self.namespace
                and metadata.get('name') == self.artifact_name and metadata.get('uid'), 'artifact resource identity absent or foreign')
        for key, expected in {'rcoder.e2e.owner': self.identity['run_id'], 'rcoder.e2e.app': self.app, 'rcoder.e2e.role': 'prod-artifacts'}.items():
            require(metadata.get('labels', {}).get(key) == expected, 'artifact resource owner label differs')

    def prepare_artifacts(self, image, create, artifact_url=None):
        require(image == RUNTIME_IMAGE, 'use the exact Root-authorized runtime OCI registry/digest, never a short Docker Hub name')
        proposals = artifact_proposal(self.namespace, self.identity['run_id'], self.app, image, self.artifacts)
        existing = [self.object(kind, self.artifact_name) for kind in ('configmap', 'pod', 'service')]
        require(not any(existing) or all(existing), 'partial artifact scene already exists; do not overwrite it')
        if not any(existing):
            require(create, 'Root must prepare owned artifact resources or explicitly enable their creation')
            for row in proposals:
                self.ownership()
                file = self.root / ('execute-artifact-' + self.app + '-' + row['kind'] + '.json')
                require(not file.exists(), 'never overwrite reviewed resource input');file.write_text(json.dumps(row))
                self.kube(['create', '-f', str(file), '-o', 'json'])
        rows = [self.object(kind, self.artifact_name) for kind in ('configmap', 'pod', 'service')]
        for row, kind in zip(rows, ('ConfigMap', 'Pod', 'Service')):self.validate_artifact_resource(row, kind)
        config, pod, service = rows
        require(config.get('immutable') is True and config.get('data', {}).get('serve.py') == ARTIFACT_SERVER, 'artifact server source differs from the frozen proposal')
        for version in ('A', 'B'):
            data = base64.b64decode(config['binaryData'][version + '.zip'])
            require(hashlib.sha256(data).hexdigest() == self.artifacts[version]['sha256'], 'actual ConfigMap artifact bytes differ')
        require(pod['spec']['containers'][0]['image'] == image and service['spec']['type'] == 'ClusterIP'
                and service['spec']['selector'] == proposals[2]['spec']['selector'], 'actual artifact compute/image/selector differs')
        self.artifact_uid = pod['metadata']['uid']
        self.artifact_base = 'http://' + self.artifact_name + '.' + self.namespace + '.svc.cluster.local:8019'
        if artifact_url:
            parsed=urllib.parse.urlsplit(artifact_url)
            require(parsed.scheme=='http' and parsed.hostname==service['spec']['clusterIP'] and parsed.port==8019
                    and parsed.path in ('','/') and not parsed.username and not parsed.password and not parsed.query and not parsed.fragment,
                    'artifact URL must be this exact captured owned ClusterIP Service')
            self.artifact_base=artifact_url.rstrip('/')
        self.report['artifact_resources'] = [resource_identity(row) for row in rows]
        self.poll('artifact server readiness', lambda: self.artifact_request('/health'), lambda data: data == b'ready', 90)

    def artifact_request(self, path, json_result=False):
        pod = self.object('pod', self.artifact_name)
        self.validate_artifact_resource(pod, 'Pod');require(pod['metadata']['uid'] == self.artifact_uid, 'artifact Pod was replaced')
        code = 'import sys,urllib.request;sys.stdout.buffer.write(urllib.request.urlopen("http://127.0.0.1:8019"+sys.argv[1],timeout=5).read())'
        raw = self.kube(['exec', self.artifact_name, '-c', 'supply', '--', 'python3', '-c', code, path], json_output=False).encode()
        return json.loads(raw) if json_result else raw

    def remaining(self,cap):
        stage=getattr(getattr(self,'observation_clock',None),'deadline',self.deadline)
        left=min(self.deadline,stage)-time.monotonic()
        require(left>0,'original parent or observation deadline ended; do not dispatch more work')
        return min(cap,left)

    def poll(self, label, probe, accepts, seconds):
        previous=getattr(self.observation_clock,'deadline',self.deadline)
        deadline=min(self.deadline,previous,time.monotonic()+seconds);last=None
        self.observation_clock.deadline=deadline
        try:
            while time.monotonic()<deadline:
                try:
                    last=probe()
                    if accepts(last):return last
                except Exception as error:last=str(safe(str(error)))
                time.sleep(min(0.5,max(0,deadline-time.monotonic())))
            raise RuntimeError(label+' exceeded its original bounded observation: '+str(safe(last)))
        finally:self.observation_clock.deadline=previous

    def prod_physical(self, running=True, expected_restart_id=None, previous_pod_uid=None):
        selector = 'app.kubernetes.io/instance=' + self.app + ',app.kubernetes.io/managed-by=rcoder-app-manager'
        rows = self.kube(['get', 'deployments,pods,replicasets', '-l', selector, '-o', 'json'])['items']
        for row in rows:
            meta = row['metadata'];require(meta.get('namespace') == self.namespace and meta.get('uid'), 'Prod resource namespace/UID missing')
        deployments = [row for row in rows if row['kind'] == 'Deployment'];pods = [row for row in rows if row['kind'] == 'Pod']
        require(len(deployments) == 1, 'Prod controller count differs')
        if not running:require(not pods,'stopped Prod still has owned Pods, including terminating ones')
        deployment = deployments[0];require(deployment['spec']['replicas'] == (1 if running else 0), 'Prod replicas differ')
        if expected_restart_id is not None:
            require(deployment['spec']['template'].get('metadata',{}).get('annotations',{}).get('rcoder.io/restart-operation')==expected_restart_id,
                    'current Prod template belongs to another restart operation')
        volumes = deployment['spec']['template']['spec']['volumes']
        claims = {v['persistentVolumeClaim']['claimName'] for v in volumes if v.get('persistentVolumeClaim')}
        require(len(claims) == 1, 'Prod must retain its exact single workspace claim')
        pvc = self.object('pvc', next(iter(claims)))
        require(pvc and pvc['metadata'].get('namespace') == self.namespace and pvc['metadata'].get('uid')
                and pvc.get('status', {}).get('phase') == 'Bound' and pvc['spec'].get('volumeName'), 'Prod PVC binding absent')
        require(pvc['metadata'].get('labels', {}).get('service_type') == 'user-app'
                and pvc['metadata'].get('labels', {}).get('app.kubernetes.io/managed-by') == 'rcoder-runtime', 'Prod PVC family/manager differs')
        require(deployment['spec']['template']['spec']['containers'][0]['image'] == RUNTIME_IMAGE, 'actual Prod runtime config differs from Root immutable image')
        value = {'deployment': resource_identity(deployment), 'template_sha256': hashlib.sha256(json.dumps(deployment['spec']['template'],sort_keys=True).encode()).hexdigest(),
                 'pvc': {**resource_identity(pvc),'volume_name':pvc['spec']['volumeName']}, 'pod': None}
        if running:
            pod,rs=current_prod_pod(deployment,pods,[row for row in rows if row['kind']=='ReplicaSet'],self.namespace,self.app)
            require(pod['metadata']['uid']!=previous_pod_uid,'restart reused the original pre-restart Pod')
            require(pod['status'].get('phase') == 'Running' and pod['status'].get('podIP'), 'current Prod Pod not running/addressed')
            statuses = pod['status'].get('containerStatuses', [])
            require(statuses and all(s.get('ready') and s.get('imageID') and s.get('containerID') for s in statuses)
                    and any(s.get('name')=='app' and isinstance(s.get('state',{}).get('running'),dict) for s in statuses), 'current Prod containers not ready/running with real image/process identities')
            require({volume['persistentVolumeClaim']['claimName'] for volume in pod.get('spec',{}).get('volumes',[]) if volume.get('persistentVolumeClaim')}==claims,
                    'current Prod Pod does not mount the captured workspace claim')
            value['replicaset']=resource_identity(rs)
            value['pod'] = {**resource_identity(pod),'ip':pod['status']['podIP'],'images':statuses}
        if self.prod_original:
            require(value['pvc']['uid'] == self.prod_original['pvc']['uid'] and value['pvc']['volume_name'] == self.prod_original['pvc']['volume_name'], 'original Prod PVC/PV was replaced')
        return value

    def content(self, snapshot, expected):
        code = 'import os,sys,urllib.request;assert os.environ.get("RCODER_PHYSICAL_POD_UID")==sys.argv[1],"captured Prod Pod changed";sys.stdout.write(urllib.request.urlopen("http://127.0.0.1:9080/",timeout=5).read().decode())'
        current = self.prod_physical();require(current['pod']['uid'] == snapshot['pod']['uid'], 'captured Prod Pod changed before HTTP probe')
        observed = self.kube(['exec', snapshot['pod']['name'], '-c','app','--', 'python3', '-c', code,snapshot['pod']['uid']], json_output=False)
        return observed == expected

    def business_ready(self, snapshot, version, seconds=180):
        def observe():
            current=self.prod_physical()
            require(current['pod']['uid']==snapshot['pod']['uid'] and current['replicaset']['uid']==snapshot['replicaset']['uid'],
                    'captured current Prod Pod/ReplicaSet changed during business observation')
            code='import json,os,sys,urllib.request;assert os.environ.get("RCODER_PHYSICAL_POD_UID")==sys.argv[1],"captured Prod Pod changed";print(json.dumps({"deploy":json.load(urllib.request.urlopen("http://127.0.0.1:3010/v1/deploy/status",timeout=5)),"ready":json.load(urllib.request.urlopen("http://127.0.0.1:3010/ready",timeout=5)),"identity":json.load(urllib.request.urlopen("http://127.0.0.1:3010/v1/runtime/identity",timeout=5))}))'
            result=json.loads(self.kube(['exec',snapshot['pod']['name'],'-c','app','--','python3','-c',code,snapshot['pod']['uid']],json_output=False))
            deploy=result['deploy'];identity=result['identity']
            require(identity.get('success') is True and identity.get('data',{}).get('application_id')==self.app and identity['data'].get('runtime_instance_id'),
                    'current management owner identity differs from the actual application')
            require(deploy.get('success') is True and deploy.get('code')=='0000' and deploy.get('data',{}).get('phase')=='running'
                    and deploy['data'].get('release_id')==self.artifacts[version]['release_id']
                    and result['ready'].get('status')=='ready' and result['ready'].get('phase')=='running',
                    'current management deployment or business readiness has not completed')
            public=self.api('/api/v1/userapp/'+self.app,cap=10)
            require(public.get('phase')=='Running' and public.get('pod_ip')==snapshot['pod']['ip'],
                    'controller has not observed the pinned ready Prod instance; hot dispatch is premature')
            require(self.content(snapshot,self.artifacts[version]['marker']),'current real business HTTP content differs')
            return {'physical':current,'management':result,'controller_runtime':public}
        return self.poll('captured current Prod management and HTTP '+version,observe,bool,seconds)

    def workspace_and_worker(self, snapshot, prepare=False):
        self.ownership();current=self.prod_physical()
        require(current['pod']['uid']==snapshot['pod']['uid'] and current['pvc']['uid']==snapshot['pvc']['uid'],'original Prod Pod/PVC changed before exec evidence')
        code = r'''import hashlib,json,os,sys
from pathlib import Path
app,marker,mode,sentinel_name,pod_uid=sys.argv[1:];root=Path('/home/user')/app
if os.environ.get('RCODER_PHYSICAL_POD_UID')!=pod_uid:raise RuntimeError('captured Prod Pod changed')
if not sentinel_name.startswith('.local-k8s-prod-sentinel-') or '/' in sentinel_name:raise RuntimeError('sentinel is not an owned basename')
sentinel=root/sentinel_name
if mode=='prepare':
 if sentinel.exists():raise RuntimeError('Prod sentinel already existed before this owned run')
 sentinel.write_text(marker)
workers=[]
for p in Path('/proc').iterdir():
 if not p.name.isdigit():continue
 try:
  argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]
  cwd=(p/'cwd').resolve();fields=(p/'stat').read_text().rsplit(')',1)[1].split()
  if cwd.name=='worker' and any(v=='main.py' or v.endswith('/main.py') for v in argv):
   lock=cwd.parent/'release.lock.toml'
   workers.append({'pid':int(p.name),'start_time':fields[19],'state':fields[0],'cwd':str(cwd),'lock_sha256':hashlib.sha256(lock.read_bytes()).hexdigest()})
 except (FileNotFoundError,PermissionError,ProcessLookupError):continue
print(json.dumps({'sentinel_matches':sentinel.read_text()==marker,'workers':workers}))
'''
        result=json.loads(self.kube(['exec',snapshot['pod']['name'],'-c','app','--','python3','-c',code,self.app,'owned-'+self.identity['run_id'],'prepare' if prepare else 'observe',self.sentinel_name,snapshot['pod']['uid']],json_output=False))
        require(result['sentinel_matches'] and len(result['workers'])==1 and result['workers'][0]['state']!='Z','original PVC data or real process worker evidence differs')
        return result

    def by_request(self, request_id, secondary=False):
        return self.api('/api/v1/userapp/' + self.app + '/operations/by-request?' + urllib.parse.urlencode({'request_id':request_id}), secondary=secondary, cap=10)

    def deploy(self, version, request_id, hot=False, gate=None, secondary=False, action='start'):
        artifact = self.artifacts[version]
        uri = '/gated/' + gate + '/' + version + '.zip' if gate else '/artifacts/' + version + '.zip'
        # Requested release tags are independent from the archive's immutable
        # manifest release ID. A unique gate tag prevents the real downloader's
        # same-release+SHA deduplication from skipping this controlled GET.
        requested_release = 'gate-' + gate if gate else artifact['release_id']
        body = {'request_id': request_id, 'lifecycle_id':self.lifecycle_id, 'url':self.artifact_base + uri,
                'release_id':requested_release,'sha256':artifact['sha256'],'auto_execute_sql':False}
        if hot:body['deploy_mode']='hot'
        data = self.api('/api/v1/userapp/' + self.app + '/' + action, body, secondary)
        require(data.get('operation_id') and data.get('release_id') == requested_release, 'actual deploy reply lacks original operation/requested release identity')
        record = operation_identity(self.by_request(request_id, secondary), self.app, self.lifecycle_id, request_id,
                                    'RestartDeployment' if action == 'restart' else 'StartDeployment')
        require(record['operation_id'] == data['operation_id'] and record['state'] == 'Succeeded', 'reply did not confirm the same real durable operation')
        self.report['original_operations'].append(record);self.persist()
        return record

    def holder(self, workers, gate):
        rid = 'hold-' + uuid.uuid4().hex
        future = workers.submit(self.deploy, 'B', rid, True, gate)
        self.poll('actual gated artifact GET', lambda:self.artifact_request('/gate/'+gate,True),lambda r:r['entered'] and not r['released'],30)
        record = self.poll('original real hot holder admission',lambda:self.by_request(rid,True),lambda r:isinstance(r,dict) and r.get('state')=='Running',30)
        operation_identity(record,self.app,self.lifecycle_id,rid,'StartDeployment')
        require(not future.done(), 'holder already completed before the real conflict window')
        lease = compact_lease(self.object('lease','rcoder-operation-prod-'+self.app),self.namespace,self.app)
        self.report['original_operations'].append(record);self.event('real_holder',{'operation':record,'lease':lease,'gate':gate})
        return future,rid,record,lease

    def release(self, gate):
        self.ownership();result=self.artifact_request('/release/'+gate,True)
        require(result['entered'] and result['released'], 'actual entered download gate was not released')

    def compute_stop(self, stage):
        rid='final-'+stage+'-'+uuid.uuid4().hex
        status, body=self.call('POST','/computer/pod/stop',{'app_id':self.app,'app_stage':stage,'service_type':'userapp','lifecycle_id':self.lifecycle_id,'request_id':rid})
        require(status==202 and body.get('code')=='0000' and body.get('success') is True,'real compute Stop was not accepted')
        op=body['data'];require(op.get('app_id')==self.app and op.get('lifecycle_id')==self.lifecycle_id and op.get('action')=='stop' and op.get('scope')==stage.capitalize()
            and op.get('operation_id')==body.get('operation_id') and op.get('status_url')=='/computer/pod/operations/'+self.app+'/'+op['operation_id'], 'compute Stop receipt identity differs')
        def observe():
            s,b=self.call('GET',op['status_url'],cap=10);require(s==200 and b.get('code')=='0000','original compute GET failed')
            row=b['data'];require(row.get('operation_id')==op['operation_id'] and row.get('action')=='stop' and row.get('lifecycle_id')==self.lifecycle_id and row.get('app_id')==self.app and row.get('scope')==stage.capitalize(),'original compute operation changed')
            require(row.get('state') in ('pending','running','succeeded'),'compute Stop failed/recovery required')
            return row
        return self.poll('original compute Stop',observe,lambda r:r.get('state')=='succeeded' and r.get('stage')=='completed',240)

    def run(self, image, create, artifact_url=None):
        self.ownership();before=self.controller_proof();self.report['inputs_before']=before
        lifecycle=self.api('/api/v1/userapp/'+self.app+'/lifecycle',cap=10)
        require(lifecycle.get('lifecycle_id') == self.lifecycle_id, 'historical Stage1 lifecycle was replaced')
        builder=self.physical(True)
        require(builder['sts']['uid']==self.original['sts']['uid'] and builder['pod']['uid']==self.original['pod']['uid'], 'historical Stage1 builder was replaced')
        if self.stopped_proof is None:
            require(not self.kube(['get','deployments','-l','app.kubernetes.io/instance='+self.app,'-o','json'])['items'],'existing Prod workload requires its explicit original Stop proof')
        else:
            ref=self.report['stopped_prod_proof'];require(digest(ref['path'])==ref['sha256'],'stopped Prod receipt bytes changed')
            stopped=self.prod_physical(False)
            failed,receipt=validate_stopped_prod(self.stopped_proof,stopped,self.app,self.namespace,self.lifecycle_id,self.namespace_uid)
            previous=self.by_request(failed['request_id'])
            require(all(previous.get(key)==failed.get(key) for key in ('operation_id','app_id','lifecycle_id','request_id','scope','kind','state')), 'original Failed operation was rewritten or replaced')
            status,body=self.call('GET',receipt['status_url'],cap=10)
            require(status==200 and body.get('code')=='0000' and body.get('success') is True
                    and all(body.get('data',{}).get(key)==receipt.get(key) for key in ('operation_id','app_id','lifecycle_id','scope','action'))
                    and body['data'].get('state')=='succeeded' and body['data'].get('stage')=='completed', 'retained compute Stop terminal cannot be re-observed')
            self.prod_original=stopped
            self.check('prior Failed cold preserved and its owned Prod compute Stop retains original PVC',True,
                       {'original_failed_operation':previous,'compute_stop_terminal':body['data'],'stopped_physical':stopped,'historical_builder':builder})
        self.prepare_artifacts(image,create,artifact_url)
        cold_id='cold-'+uuid.uuid4().hex
        require(self.stopped_proof is None or cold_id != self.stopped_proof['original_failed_operation']['request_id'], 'new cold request must not replay old failure')
        cold=self.deploy('A',cold_id)
        original=self.poll('cold real Prod physical identity',self.prod_physical,lambda r:bool(r['pod']),180)
        self.prod_original=original
        self.poll('real business A HTTP',lambda:self.content(original,self.artifacts['A']['marker']),bool,60)
        cold_data=self.workspace_and_worker(original,True)
        require(cold_data['workers'][0]['lock_sha256']==self.artifacts['A']['release_lock_sha256'],'actual worker lock differs from original built A')
        self.check('cold actual operation business HTTP worker and PVC data complete',True,{'operation':cold,'physical':original,'data':cold_data})
        replay=self.deploy('A',cold_id)
        require(replay['operation_id']==cold['operation_id'] and self.prod_physical()['pod']['uid']==original['pod']['uid'],'same request redeployed or changed its original operation')
        self.check('same cold request replays without new Pod or operation',True,replay)
        # Existing host reach is a real prerequisite of the product's cold/hot
        # paths. Probe actual addresses; do not substitute exec-only success.
        for port in (3010,60000):
            route='/v1/deploy/status' if port==3010 else '/health'
            probe=subprocess.run(['curl','--silent','--show-error','--noproxy','*','--max-time','5','--fail','http://'+original['pod']['ip']+':'+str(port)+route],capture_output=True,timeout=self.remaining(6))
            require(probe.returncode==0,'host cannot reach actual Prod Pod management/file port; inside-master topology is required')
        self.check('host reaches actual Prod management and file ports',True,{'pod_uid':original['pod']['uid'],'ip':original['pod']['ip']})
        # Threads retain their original dispatched request. Failure never sends
        # a release/Stop/delete to turn an uncertain result into a fake pass.
        workers=ThreadPoolExecutor(max_workers=3)
        try:
            gate='r2-'+uuid.uuid4().hex[:16];future,rid,holder,lease=self.holder(workers,gate)
            self.check('old real business stays healthy while hot download is gated',self.content(original,self.artifacts['A']['marker']),{'pod_uid':original['pod']['uid'],'holder':holder})
            for other in (False,True):
                started=time.monotonic()
                denied_stop='busy-'+uuid.uuid4().hex
                status,body=self.call('POST','/api/v1/userapp/'+self.app+'/stop?'+urllib.parse.urlencode({'request_id':denied_stop,'lifecycle_id':self.lifecycle_id}),{},other,10)
                require(status==200,'formal busy business Stop transport must remain HTTP200');validate_busy(body,holder['operation_id'])
                require(time.monotonic()-started<10 and not future.done() and self.by_request(denied_stop,other) is None,'busy Stop waited/admitted instead of rejecting the original holder')
                self.check('real Lease R2 busy on '+('secondary' if other else 'primary'),True,{'body':body,'holder':holder,'lease':lease})
            self.release(gate);future.result(timeout=self.remaining(180))
            self.poll('real hot B HTTP',lambda:self.content(original,self.artifacts['B']['marker']),bool,60)
            hot_data=self.workspace_and_worker(original)
            require(hot_data['workers'][0]['lock_sha256']==self.artifacts['B']['release_lock_sha256'],'real hot worker did not execute built B')
            self.check('hot actual activation preserves original Pod PVC data and real worker',self.prod_physical()['pod']['uid']==original['pod']['uid'],{'physical':self.prod_physical(),'data':hot_data})
            gate='r1-release-'+uuid.uuid4().hex[:12];future,rid,holder,lease=self.holder(workers,gate)
            wait_id='wait-'+uuid.uuid4().hex;started=time.monotonic()
            waiting=workers.submit(self.api,'/api/v1/userapp/'+self.app+'/restart',{'request_id':wait_id,'lifecycle_id':self.lifecycle_id},True,90)
            time.sleep(3)
            require(not waiting.done() and self.by_request(wait_id,True) is None,'R1 did not wait before actual business admission')
            self.release(gate)
            accepted=self.poll('R1 admission after original real lease release',lambda:self.by_request(wait_id,True),lambda r:isinstance(r,dict),max(0.1,30-(time.monotonic()-started)))
            operation_identity(accepted,self.app,self.lifecycle_id,wait_id,'RestartDeployment')
            admission_elapsed=time.monotonic()-started
            require(admission_elapsed<30,'R1 reopened or exceeded its original admission deadline')
            future.result(timeout=self.remaining(180));answer=waiting.result(timeout=self.remaining(180))
            record=operation_identity(self.by_request(wait_id,True),self.app,self.lifecycle_id,wait_id,'RestartDeployment')
            require(record['state']=='Succeeded' and answer.get('operation_id')==record['operation_id'] and accepted['operation_id']==record['operation_id'],'released R1 did not finish the one original admitted request')
            self.check('R1 released real holder resumes same original request',True,{'operation':record,'admission_elapsed':admission_elapsed,'execution_elapsed':time.monotonic()-started,
                       'instruction_result':{'phase':answer.get('phase'),'pod_ip':answer.get('pod_ip')},'success_boundary':'captured rollout PATCH acknowledged; business readiness is a separate following stage'})
            current=self.poll('original restart new current Prod Pod',lambda:self.prod_physical(expected_restart_id=record['operation_id'],previous_pod_uid=original['pod']['uid']),lambda r:bool(r['pod']),180)
            ready=self.business_ready(current,'B',180)
            self.check('R1 original restart has new current Pod and actual management plus HTTP B ready',True,ready)
            self.check('R1 completion preserves actual original PVC data',True,self.workspace_and_worker(current))
            gate='r1-expire-'+uuid.uuid4().hex[:12];future,rid,holder,lease=self.holder(workers,gate)
            denied_id='expire-'+uuid.uuid4().hex;started=time.monotonic()
            status,body=self.call('POST','/api/v1/userapp/'+self.app+'/restart',{'request_id':denied_id,'lifecycle_id':self.lifecycle_id},True,45)
            elapsed=time.monotonic()-started
            require(status==200 and 29<=elapsed<=38,'R1 must exhaust the real default30s admission deadline');validate_busy(body,holder['operation_id'])
            require(self.by_request(denied_id,True) is None and not future.done(),'exhausted R1 admitted or displaced original holder')
            self.release(gate);future.result(timeout=self.remaining(180));time.sleep(2)
            require(self.by_request(denied_id,True) is None and self.prod_physical()['pod']['uid']==current['pod']['uid'],'rejected R1 executed late after lease release')
            self.check('R1 real30s timeout has original holder and no late admission',True,{'body':body,'elapsed':elapsed,'holder':holder,'lease':lease})
        finally:workers.shutdown(wait=False,cancel_futures=False)
        stopped=self.compute_stop('prod');physical=self.poll('final Prod physical Stop',lambda:self.prod_physical(False),bool,180)
        builder_stopped=self.compute_stop('dev')
        builder=self.poll('final builder compute Stop retaining original workspace',lambda:self.physical(False),bool,180)
        after=self.controller_proof();require(before==after,'controller binary/source/process or harness changed during execution')
        self.check('successful final compute Stops retain all application PVCs',True,{'prod':stopped,'builder':builder_stopped,'prod_physical':physical,'builder_physical':builder})
        self.report.update(success=True,inputs_after=after);self.persist()

    def failure_evidence(self,error):
        self.report['success']=False;self.report['error']=safe(str(error));self.report['cleanup']='not attempted: keep all original live requests/resources/PVCs'
        self.report['failure_resources']={}
        for kind,selector in [('deployments','app.kubernetes.io/instance='+self.app),('pods','app.kubernetes.io/instance='+self.app),('pvc','service_type=user-app')]:
            try:
                rows=self.kube(['get',kind,'-l',selector,'-o','json'])['items'];self.report['failure_resources'][kind]=[resource_identity(r) for r in rows]
            except Exception as problem:self.report['failure_resources'][kind]={'error':safe(str(problem))}
        try:self.report['failure_lease']=compact_lease(self.object('lease','rcoder-operation-prod-'+self.app),self.namespace,self.app)
        except Exception as problem:self.report['failure_lease']={'error':safe(str(problem))}
        self.persist()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',required=True,type=Path);parser.add_argument('--primary',required=True);parser.add_argument('--secondary','--peer-url',dest='secondary',required=True);parser.add_argument('--app',required=True)
    parser.add_argument('--artifact-image',required=True);parser.add_argument('--create-artifacts',action='store_true',help='Explicitly create only this run-owned artifact ConfigMap/Pod/Service; never overwrite')
    parser.add_argument('--build-report',type=Path,help='Exact passed Stage1/resume receipt within the owned run; original report is never overwritten')
    parser.add_argument('--artifact-url',help='Optional exact captured owned ClusterIP:8019 URL; arbitrary external download origins are refused')
    parser.add_argument('--report-suffix',help='Explicit new evidence suffix; defaults to a fresh UUID and never overwrites a failed run')
    parser.add_argument('--controller-revision-proof',type=Path,help='Root retained build/process/before/after receipt authorizing historical artifacts on a newer controller')
    parser.add_argument('--stopped-prod-proof',type=Path,help='Original Failed operation plus actual completed compute Stop and immutable stopped Deployment/PVC proof')
    parser.add_argument('--authorize-owned-mutations',action='store_true');parser.add_argument('--budget-seconds',type=int,default=1800)
    args=parser.parse_args();require(args.authorize_owned_mutations,'Root must explicitly authorize real owned execution')
    require(600<=args.budget_seconds<=3600,'bounded total execution budget required');harness=None
    try:
        harness=Execute(args.root,args.primary,args.secondary,args.app,args.budget_seconds,args.build_report,args.report_suffix,args.controller_revision_proof,args.stopped_prod_proof);harness.run(args.artifact_image,args.create_artifacts,args.artifact_url)
    except Exception as error:
        if harness:harness.failure_evidence(error)
        print(json.dumps({'success':False,'error':safe(str(error))}),flush=True);return 1
    print(json.dumps({'success':harness.report['success'],'report':str(harness.report_path)}),flush=True);return 0


if __name__=='__main__':raise SystemExit(main())
