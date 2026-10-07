"""Execution protocol/safety units only; no Kubernetes, HTTP or product success."""
import datetime
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

import local_k8s_prod_execute as execute


class IdentityTests(unittest.TestCase):
    def test_real_stage1_schema_requires_actual_passed_before_after_check(self):
        before={'source':{'source_inputs_sha256':'a'*64},'binary_sha256':'b'*64,'host_pid':42}
        report={'inputs_before':before,'checks':[{'name':'build source host binary and harness remain frozen','passed':True,'evidence':{'before':before,'after':before.copy()}}]}
        self.assertNotIn('inputs_after',report)
        self.assertEqual(execute.stage1_frozen_proof(report),before)
        self.assertNotIn('inputs_after',report,'strict consumer must never backfill the original report')
        for changed in [{'checks':[]},{'inputs_before':{**before,'host_pid':84}}, {'inputs_after':{**before,'host_pid':84}}]:
            with self.subTest(changed=changed),self.assertRaises(RuntimeError):execute.stage1_frozen_proof({**report,**changed})

    def record(self):
        return {'app_id':'ownedapp','lifecycle_id':'original-life','request_id':'original-request',
                'operation_id':'original-op','scope':'Prod','kind':'StartDeployment','state':'Running'}

    def test_original_operation_identity_cannot_be_substituted_or_recovery_success(self):
        record=self.record()
        self.assertEqual(execute.operation_identity(record,'ownedapp','original-life','original-request','StartDeployment'),record)
        for changed in [{'app_id':'other'},{'lifecycle_id':'other-life'},{'request_id':'later-request'},
                        {'operation_id':None},{'scope':'Dev'},{'kind':'RestartDeployment'},{'state':'Failed'},{'state':'RecoveryRequired'}]:
            with self.subTest(changed=changed),self.assertRaises(execute.ContractFailure if hasattr(execute,'ContractFailure') else RuntimeError):
                execute.operation_identity({**record,**changed},'ownedapp','original-life','original-request','StartDeployment')

    def lease(self):
        return {'metadata':{'name':'rcoder-operation-prod-ownedapp','namespace':'rcoder-owned','uid':'physical-lease-uid','resourceVersion':'7',
                           'labels':{'rcoder.io/operation-app':'ownedapp','rcoder.io/operation-family':'user-app'}},
                'spec':{'holderIdentity':'private-physical-holder','renewTime':datetime.datetime.now(datetime.timezone.utc).isoformat(),'leaseDurationSeconds':60}}

    def test_actual_live_lease_uid_and_private_token_hash_are_required(self):
        row=self.lease();proof=execute.compact_lease(row,'rcoder-owned','ownedapp')
        self.assertEqual(proof['uid'],'physical-lease-uid')
        self.assertNotIn('private-physical-holder',str(proof));self.assertEqual(len(proof['holder_sha256']),64)
        for changed in [{'uid':None},{'namespace':'rcoder-dev'},{'name':'other-lease'}]:
            with self.subTest(changed=changed),self.assertRaises(RuntimeError):
                execute.compact_lease({**row,'metadata':{**row['metadata'],**changed}},'rcoder-owned','ownedapp')
        row['spec']['renewTime']=(datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta(seconds=61)).isoformat()
        with self.assertRaises(RuntimeError):execute.compact_lease(row,'rcoder-owned','ownedapp')

    def test_short_registry_ref_fails_before_any_resource_get_or_create(self):
        harness=execute.Execute.__new__(execute.Execute)
        harness.object=Mock(side_effect=AssertionError('no resource call permitted'))
        with self.assertRaises(RuntimeError):harness.prepare_artifacts('nuwax-test/app-runtime@sha256:'+'a'*64,True)
        harness.object.assert_not_called()

    def test_gated_deploy_has_a_unique_request_release_but_original_zip_sha(self):
        harness=execute.Execute.__new__(execute.Execute)
        harness.app='ownedapp';harness.lifecycle_id='original-life';harness.artifact_base='http://owned.svc:8019'
        harness.artifacts={'B':{'release_id':'actual-manifest-release','sha256':'a'*64}}
        harness.report={'original_operations':[]};harness.persist=Mock()
        def api(route,body,secondary):
            self.assertEqual(body['release_id'],'gate-r1-owned')
            self.assertEqual(body['url'],'http://owned.svc:8019/gated/r1-owned/B.zip')
            self.assertEqual(body['sha256'],'a'*64)
            return {'operation_id':'original-op','release_id':body['release_id']}
        harness.api=api
        harness.by_request=Mock(return_value={**self.record(),'state':'Succeeded'})
        result=harness.deploy('B','original-request',True,'r1-owned')
        self.assertEqual(result['operation_id'],'original-op')


class RevisionTests(unittest.TestCase):
    def test_explicit_revision_keeps_distinct_real_artifact_and_controller_sources(self):
        historical = {'source': {'source_inputs_sha256': 'a'*64}}
        current = {'source': {'source_inputs_sha256': 'b'*64}, 'source_bound_to_build': True}
        proof = {'schema_version': 1, 'app_id': 'ownedapp', 'namespace': 'rcoder-owned', 'run_id': 'c'*32,
                 'lifecycle_id': 'original-life', 'stage1_report': {'sha256': 'd'*64},
                 'historical_source_inputs_sha256': 'a'*64, 'controller_source_inputs_sha256': 'b'*64}
        execute.validate_controller_revision(proof, historical, current, 'd'*64, 'ownedapp', 'rcoder-owned', 'c'*32, 'original-life')
        for change in [{'app_id': 'other'}, {'namespace': 'rcoder-dev'}, {'historical_source_inputs_sha256': 'b'*64},
                       {'controller_source_inputs_sha256': 'a'*64}, {'stage1_report': {'sha256': 'e'*64}}]:
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                execute.validate_controller_revision({**proof, **change}, historical, current, 'd'*64, 'ownedapp', 'rcoder-owned', 'c'*32, 'original-life')
        with self.assertRaises(RuntimeError):
            execute.validate_controller_revision(proof, historical, {**current, 'source_bound_to_build': False}, 'd'*64, 'ownedapp', 'rcoder-owned', 'c'*32, 'original-life')

    def stopped(self):
        snapshot = {'deployment': {'uid': 'original-deployment'}, 'pvc': {'uid': 'original-pvc', 'volume_name': 'original-pv'}, 'pod': None}
        failed = {'app_id': 'ownedapp', 'lifecycle_id': 'original-life', 'scope': 'Prod', 'kind': 'StartDeployment',
                  'state': 'Failed', 'operation_id': 'original-failed-cold', 'request_id': 'old-cold-request'}
        stop = {'app_id': 'ownedapp', 'lifecycle_id': 'original-life', 'scope': 'Prod', 'action': 'stop',
                'operation_id': 'original-compute-stop', 'status_url': '/computer/pod/operations/ownedapp/original-compute-stop'}
        proof = {'schema_version': 1, 'app_id': 'ownedapp', 'namespace': 'rcoder-owned', 'lifecycle_id': 'original-life',
                 'namespace_uid': 'original-namespace', 'original_failed_operation': failed, 'compute_stop_receipt': stop,
                 'compute_stop_terminal': {**stop, 'state': 'succeeded', 'stage': 'completed'}, 'stopped_physical': snapshot}
        return proof, snapshot

    def test_stopped_resume_never_rewrites_failure_or_replaces_original_pvc(self):
        proof, snapshot = self.stopped()
        failed, stop = execute.validate_stopped_prod(proof, snapshot, 'ownedapp', 'rcoder-owned', 'original-life', 'original-namespace')
        self.assertEqual(failed['state'], 'Failed')
        self.assertNotEqual(failed['operation_id'], stop['operation_id'])
        for change in [{'original_failed_operation': {**failed, 'state': 'Succeeded'}},
                       {'namespace_uid': 'replacement'}, {'compute_stop_terminal': {**proof['compute_stop_terminal'], 'state': 'running'}},
                       {'compute_stop_terminal': {**proof['compute_stop_terminal'], 'operation_id': 'later-stop'}}]:
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                execute.validate_stopped_prod({**proof, **change}, snapshot, 'ownedapp', 'rcoder-owned', 'original-life', 'original-namespace')
        for change in [{'pvc': {'uid': 'replacement', 'volume_name': 'original-pv'}}, {'pvc': {'uid': 'original-pvc', 'volume_name': 'replacement'}},
                       {'pod': {'uid': 'still-running'}}, {'deployment': {'uid': 'replacement'}}]:
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                execute.validate_stopped_prod(proof, {**snapshot, **change}, 'ownedapp', 'rcoder-owned', 'original-life', 'original-namespace')

    def test_receipt_hash_and_root_boundary_are_checked_before_consumption(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            receipt = root / 'actual.json'
            receipt.write_text(json.dumps({'cargo_exit': 0}))
            reference = {'path': str(receipt), 'sha256': hashlib.sha256(receipt.read_bytes()).hexdigest()}
            self.assertEqual(execute.owned_receipt(root, reference, 'actual'), {'cargo_exit': 0})
            with self.assertRaises(RuntimeError):
                execute.owned_receipt(root, {**reference, 'sha256': 'a'*64}, 'changed')
            outside = root.parent / (root.name + '-outside.json')
            outside.write_text('{}')
            try:
                link = root / 'escaped.json'
                link.symlink_to(outside)
                with self.assertRaises(RuntimeError):
                    execute.owned_receipt(root, {'path': str(link), 'sha256': hashlib.sha256(outside.read_bytes()).hexdigest()}, 'escaped')
            finally:
                outside.unlink()


class CurrentPodTests(unittest.TestCase):
    def fixture(self, restart='original-restart'):
        app, namespace = 'ownedapp', 'rcoder-owned'
        labels = {'app.kubernetes.io/instance': app, 'app.kubernetes.io/managed-by': 'rcoder-app-manager', 'rcoder.io/app-id': app}
        annotations = {'rcoder.io/deploy-template-token': 'original-deployment-op', 'rcoder.io/restart-operation': restart}
        template = {'metadata': {'labels': labels, 'annotations': annotations},
                    'spec': {'containers': [{'name': 'app', 'image': execute.RUNTIME_IMAGE}],
                             'volumes': [{'name': 'workspace', 'persistentVolumeClaim': {'claimName': 'original-pvc'}}]}}
        deployment = {'kind': 'Deployment', 'metadata': {'name': 'original-deployment', 'namespace': namespace, 'uid': 'deployment-uid', 'resourceVersion': '1'},
                      'spec': {'replicas': 1, 'selector': {'matchLabels': labels}, 'template': template}}
        rs = {'kind': 'ReplicaSet', 'metadata': {'name': 'current-rs', 'namespace': namespace, 'uid': 'current-rs-uid', 'resourceVersion': '2',
                                               'ownerReferences': [{'apiVersion': 'apps/v1', 'kind': 'Deployment', 'name': 'original-deployment', 'uid': 'deployment-uid', 'controller': True}]},
              'spec': {'template': copy.deepcopy(template)}}
        rs['spec']['template']['metadata']['labels']['pod-template-hash'] = 'current-hash'
        pod = {'kind': 'Pod', 'metadata': {'name': 'current-pod', 'namespace': namespace, 'uid': 'current-pod-uid', 'resourceVersion': '3', 'labels': labels,
                                         'annotations': annotations, 'ownerReferences': [{'apiVersion': 'apps/v1', 'kind': 'ReplicaSet', 'name': 'current-rs', 'uid': 'current-rs-uid', 'controller': True}]},
               'spec': copy.deepcopy(template['spec']), 'status': {'phase': 'Running', 'podIP': '192.0.2.99',
               'containerStatuses': [{'name': 'app', 'ready': True, 'imageID': 'sha256:runtime', 'containerID': 'containerd://current', 'state': {'running': {}}}]}}
        pvc = {'metadata': {'name': 'original-pvc', 'namespace': namespace, 'uid': 'pvc-uid', 'resourceVersion': '4',
                            'labels': {'service_type': 'user-app', 'app.kubernetes.io/managed-by': 'rcoder-runtime'}},
               'status': {'phase': 'Bound'}, 'spec': {'volumeName': 'original-pv'}}
        harness = execute.Execute.__new__(execute.Execute)
        harness.app, harness.namespace, harness.prod_original = app, namespace, None
        harness.object = Mock(return_value=pvc)
        harness.kube = Mock(return_value={'items': [deployment, rs, pod]})
        return harness, deployment, rs, pod

    def test_current_restart_template_rejects_a_ready_old_pod(self):
        harness, deployment, rs, pod = self.fixture()
        deployment['spec']['template']['metadata']['annotations']['rcoder.io/restart-operation'] = 'new-restart'
        # One old Pod is still Running/Ready after PATCH ACK. Deployment UID and
        # the deploy token are unchanged, so neither proves the new restart.
        with self.assertRaises(RuntimeError):
            harness.prod_physical()

    def test_terminating_pod_never_proves_restart_business_ready(self):
        harness, _, _, pod = self.fixture()
        pod['metadata']['deletionTimestamp'] = '2026-10-07T00:00:00Z'
        with self.assertRaises(RuntimeError):
            harness.prod_physical()

    def test_current_pod_ignores_stale_rows_but_verifies_rs_uid(self):
        harness, deployment, rs, pod = self.fixture()
        old = copy.deepcopy(pod)
        old['metadata'].update(name='old-pod', uid='old-pod-uid', deletionTimestamp='2026-10-07T00:00:00Z')
        harness.kube.return_value['items'].insert(0, old)
        result = harness.prod_physical()
        self.assertEqual(result['pod']['uid'], 'current-pod-uid')
        self.assertEqual(result['replicaset']['uid'], 'current-rs-uid')
        rs['metadata']['uid'] = 'replacement-rs-uid'
        with self.assertRaises(RuntimeError):
            harness.prod_physical()

    def test_pinned_restart_requires_new_uid_and_original_restart_annotation(self):
        harness, _, _, _ = self.fixture()
        result = harness.prod_physical(expected_restart_id='original-restart', previous_pod_uid='old-pod-uid')
        self.assertEqual(result['pod']['uid'], 'current-pod-uid')
        for arguments in [{'expected_restart_id': 'another-restart'}, {'previous_pod_uid': 'current-pod-uid'}]:
            with self.subTest(arguments=arguments), self.assertRaises(RuntimeError):
                harness.prod_physical(**arguments)


if __name__=='__main__':unittest.main()
