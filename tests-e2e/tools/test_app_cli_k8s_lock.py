#!/usr/bin/env python3
"""只验证工具契约与受控 kubectl 响应，不代表真实 CephFS 验收。"""

import importlib.util
import json
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    'app_cli_k8s_lock', Path(__file__).with_name('app_cli_k8s_lock.py'))
TOOL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TOOL)

CURRENT_SOURCE = {'origin_head': 'c' * 40, 'worktree_sha256': 'd' * 64}


def nodes(ready=True):
    return {'items': [{'metadata': {'name': name}, 'spec': {}, 'status': {
        'conditions': [{'type': 'Ready', 'status': 'True' if ready else 'False'}]}}
        for name in ('node-a', 'node-b')]}


class ControlledKubectl:
    def __init__(self):
        self.calls = []
        self.experiment = None
        self.pod_uid = 'pod-original'
        self.pod_rv = '12'
        self.pod_exists = True
        self.delete_error = None

    def __call__(self, argv, **kwargs):
        self.calls.append((argv, kwargs))
        prefix = ['kubectl', '--context', 'personal-test']
        command = argv[len(prefix):]
        if command[:2] == ['-n', self.experiment.namespace]:
            command = command[2:]
        result = None
        if command[:2] == ['get', 'nodes']:
            result = nodes()
        elif command[:2] == ['create', '-f']:
            result = json.loads(kwargs['input'])
            result['metadata']['uid'] = 'namespace-original'
        elif command[:3] == ['get', 'namespace', self.experiment.namespace]:
            result = {'metadata': {'uid': 'namespace-original'},
                      'status': {'phase': 'Active'}}
        elif command[:2] == ['get', 'pod']:
            if self.pod_exists:
                result = {'metadata': {'uid': self.pod_uid,
                    'resourceVersion': self.pod_rv, 'labels': {
                        'rcoder.io/lock-test-run': self.experiment.run_id}}}
        elif command[:2] == ['delete', '--raw']:
            if self.delete_error:
                raise subprocess.CalledProcessError(1, argv, stderr=self.delete_error)
            self.pod_exists = False
            result = {'kind': 'Status', 'status': 'Success'}
        elif command[:2] == ['get', 'pvc']:
            result = {'metadata': {'uid': 'pvc-original'},
                      'spec': {'volumeName': 'pv-original'},
                      'status': {'phase': 'Bound'}}
        else:
            raise AssertionError(f'unexpected kubectl command: {command}')
        return subprocess.CompletedProcess(argv, 0,
            stdout=json.dumps(result) if result is not None else '', stderr='')


class K8sLockToolTests(unittest.TestCase):
    def make_experiment(self, report='unused.json', **overrides):
        values = dict(context='personal-test', scenario='cephfs-lock',
                      node=['node-a', 'node-b'], image='repo/test@sha256:' + 'a' * 64,
                      build_receipt='unused-receipt.json', cephfs_class='cephfs',
                      storage_class='ceph-rbd', cleanup_volume=False,
                      node_ssh=[], report=report, source_dir=str(Path(__file__).resolve().parents[2]))
        values.update(overrides)
        args = SimpleNamespace(**values)
        controlled = ControlledKubectl()
        experiment = TOOL.ClusterExperiment(args, runner=controlled, sleep=lambda _: None)
        controlled.experiment = experiment
        return experiment, controlled

    def captured_resources(self, experiment):
        experiment.namespace_uid = 'namespace-original'
        experiment.pods['fs-a'] = {'uid': 'pod-original', 'resource_version': '1'}
        experiment.volumes['shared'] = {'uid': 'pvc-original'}

    def test_cleanup_deletes_exact_pod_and_retains_namespace_and_pvc(self):
        experiment, controlled = self.make_experiment()
        self.captured_resources(experiment)
        self.assertTrue(experiment.cleanup())
        deletes = [call for call in controlled.calls if 'delete' in call[0]]
        self.assertEqual(len(deletes), 1)
        argv, kwargs = deletes[0]
        self.assertIn('/api/v1/namespaces/' + experiment.namespace + '/pods/fs-a', argv)
        self.assertEqual(json.loads(kwargs['input'])['preconditions'],
                         {'uid': 'pod-original', 'resourceVersion': '12'})
        self.assertFalse(any('namespace' in argv or 'pvc' in argv for argv, _ in deletes))
        self.assertEqual(experiment.report['retained_namespace']['uid'], 'namespace-original')
        self.assertEqual(experiment.report['retained_volumes'], [{
            'name': 'shared', 'uid': 'pvc-original', 'phase': 'Bound',
            'volume_name': 'pv-original'}])

    def test_replacement_uid_is_never_deleted(self):
        experiment, controlled = self.make_experiment()
        self.captured_resources(experiment)
        controlled.pod_uid = 'another-pod'
        self.assertFalse(experiment.cleanup())
        self.assertFalse(any('delete' in argv for argv, _ in controlled.calls))
        self.assertIn('identity changed', experiment.report['cleanup_errors'][0])
        self.assertIn('fs-a', experiment.pods)
        self.assertEqual(experiment.report['retained_volumes'][0]['uid'], 'pvc-original')

    def test_delete_precondition_error_is_reported_and_does_not_delete_other_resources(self):
        experiment, controlled = self.make_experiment()
        self.captured_resources(experiment)
        controlled.delete_error = 'Conflict: resourceVersion precondition failed'
        self.assertFalse(experiment.cleanup())
        self.assertIn('resourceVersion', experiment.report['cleanup_errors'][0])
        self.assertIn('fs-a', experiment.pods)
        self.assertEqual(experiment.report['retained_volumes'][0]['phase'], 'Bound')

    def test_cleanup_failure_overrides_all_passed_checks_and_always_writes_report(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'nested' / 'report.json')
            experiment, controlled = self.make_experiment(report=path)
            controlled.delete_error = 'Forbidden: captured Pod cannot be deleted'

            def exercise(_):
                self.captured_resources(experiment)
                experiment.report['checks'] = [
                    {'name': name, 'ok': True} for name in TOOL.CEPHFS_STEPS]

            receipt = {'schema_version': 1, 'image': experiment.args.image,
                       'source_commit': CURRENT_SOURCE['origin_head'],
                       'source_digest': CURRENT_SOURCE['worktree_sha256']}
            with patch.object(TOOL, 'load_build_receipt', return_value=receipt), \
                    patch.object(TOOL, 'source_identity', return_value=CURRENT_SOURCE), \
                    patch.object(experiment, 'run_cephfs', side_effect=exercise):
                self.assertEqual(experiment.run(), 1)
            report = json.loads(Path(path).read_text())
            self.assertFalse(report['success'])
            self.assertFalse(report['cleanup_ok'])
            self.assertIn('Forbidden', report['cleanup_errors'][0])
            self.assertEqual(report['retained_volumes'][0]['uid'], 'pvc-original')

    def test_scenario_exception_still_writes_report_and_preserves_pvc(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'report.json')
            experiment, controlled = self.make_experiment(report=path)

            def fail(_):
                self.captured_resources(experiment)
                raise RuntimeError('owner acquisition failed')

            receipt = {'source_commit': CURRENT_SOURCE['origin_head'],
                       'source_digest': CURRENT_SOURCE['worktree_sha256']}
            with patch.object(TOOL, 'load_build_receipt', return_value=receipt), \
                    patch.object(TOOL, 'source_identity', return_value=CURRENT_SOURCE), \
                    patch.object(experiment, 'run_cephfs', side_effect=fail), \
                    patch.object(experiment, 'diagnose'):
                self.assertEqual(experiment.run(), 1)
            report = json.loads(Path(path).read_text())
            self.assertFalse(report['success'])
            self.assertIn('owner acquisition', report['error'])
            self.assertEqual(report['retained_volumes'][0]['phase'], 'Bound')
            self.assertFalse(any('delete' in argv and ('namespace' in argv or 'pvc' in argv)
                                 for argv, _ in controlled.calls))

    def test_node_selection_requires_two_distinct_ready_schedulable_nodes(self):
        self.assertEqual(TOOL.select_nodes(nodes(), ['node-a', 'node-b']), ['node-a', 'node-b'])
        for selected in (['node-a'], ['node-a', 'node-a'], ['node-a', 'unknown']):
            with self.assertRaises(ValueError):
                TOOL.select_nodes(nodes(), selected)
        with self.assertRaisesRegex(ValueError, 'not Ready'):
            TOOL.select_nodes(nodes(False), ['node-a', 'node-b'])
        document = nodes()
        document['items'][0]['spec']['unschedulable'] = True
        with self.assertRaisesRegex(ValueError, 'unschedulable'):
            TOOL.select_nodes(document, ['node-a', 'node-b'])

    def test_missing_duplicate_and_failed_required_steps_cannot_pass(self):
        checks = [{'name': name, 'ok': True} for name in TOOL.REQUIRED_STEPS['cephfs-lock']]
        TOOL.validate_required_steps({'checks': checks}, 'cephfs-lock')
        for changed in (checks[:-1], checks + checks[:1],
                        checks[:-1] + [{'name': checks[-1]['name'], 'ok': False}]):
            with self.assertRaisesRegex(RuntimeError, 'incomplete acceptance'):
                TOOL.validate_required_steps({'checks': changed}, 'cephfs-lock')
        with self.assertRaises(RuntimeError):
            TOOL.validate_required_steps({'checks': checks}, 'all')

    def test_cleanup_volume_rejected_before_any_cluster_call_and_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'report.json')
            experiment, controlled = self.make_experiment(report=path, cleanup_volume=True)
            self.assertEqual(experiment.run(), 1)
            self.assertEqual(controlled.calls, [])
            report = json.loads(Path(path).read_text())
            self.assertFalse(report['success'])
            self.assertIn('--cleanup-volume is forbidden', report['error'])

    def test_build_receipt_requires_digest_binary_and_source_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'receipt.json'
            image = 'repo/test@sha256:' + 'a' * 64
            value = {'schema_version': 1, 'image': image, 'app_cli_sha256': 'b' * 64,
                     'source_commit': 'c' * 40, 'source_digest': 'd' * 64}
            path.write_text(json.dumps(value))
            self.assertEqual(TOOL.load_build_receipt(path, image, CURRENT_SOURCE), value)
            for key in ('app_cli_sha256', 'source_commit', 'source_digest'):
                broken = dict(value)
                broken[key] = ''
                path.write_text(json.dumps(broken))
                with self.assertRaises(ValueError):
                    TOOL.load_build_receipt(path, image, CURRENT_SOURCE)
            value['image'] = 'repo/test:latest'
            path.write_text(json.dumps(value))
            with self.assertRaisesRegex(ValueError, 'immutable'):
                TOOL.load_build_receipt(path, value['image'], CURRENT_SOURCE)

    def test_uncaptured_create_response_does_not_authorize_deletion_or_success(self):
        experiment, controlled = self.make_experiment()
        experiment.unconfirmed_creations = [{'kind': 'Pod', 'name': 'fs-a'}]
        self.assertFalse(experiment.cleanup())
        self.assertFalse(any('delete' in argv for argv, _ in controlled.calls))
        self.assertIn('no captured UID', experiment.report['cleanup_errors'][0])

    def test_create_response_loss_is_persisted_as_unknown_cleanup(self):
        experiment, controlled = self.make_experiment()
        manifest = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': 'fs-a'}}
        with patch.object(experiment, 'kubectl', side_effect=subprocess.TimeoutExpired(
                ['kubectl', 'create'], 180)):
            with self.assertRaises(subprocess.TimeoutExpired):
                experiment.create(manifest)
        self.assertFalse(experiment.cleanup())
        self.assertEqual(experiment.report['unconfirmed_creations'],
                         [{'kind': 'Pod', 'name': 'fs-a'}])
        self.assertFalse(any('delete' in argv for argv, _ in controlled.calls))

    def test_contender_listener_is_rejected_before_terminal_dispatch_text(self):
        experiment, _ = self.make_experiment()
        with patch.object(experiment, 'create'), patch.object(experiment, 'wait_ready'), \
                patch.object(experiment, 'no_management_listener', return_value=[3999]):
            with self.assertRaisesRegex(RuntimeError, 'independent management listener'):
                experiment.compete('fs-b', 'node-b', 'shared')

    def test_remote_python_programs_are_syntactically_valid_and_pidfd_is_used(self):
        experiment, _ = self.make_experiment()
        captured = {'pid': 123, 'starttime': '12345', 'cmdline': 'app-cli serve '}
        commands = []

        def execute(_, command):
            commands.append(command)
            program = shlex.split(command)[2]
            compile(program, '<pod-fixture>', 'exec')
            return SimpleNamespace(stdout='{}')

        with patch.object(experiment, 'exec_in', side_effect=execute):
            experiment.observe_owner('fs-a')
            experiment.kill_exact_owner('fs-a', captured)
            experiment.no_management_listener('fs-b')
            experiment.pid1_epoch('fs-a')
        self.assertIn('os.pidfd_open(pid)', commands[1])
        self.assertIn('signal.pidfd_send_signal', commands[1])
        self.assertNotIn('pkill', commands[1])

    def test_wait_ready_rejects_wrong_node_pvc_and_image_id(self):
        experiment, _ = self.make_experiment()
        experiment.pods['fs-a'] = {'uid': 'pod-original'}
        experiment.volumes['shared'] = {'uid': 'pvc-original'}
        experiment.receipt = {'app_cli_sha256': 'b' * 64}
        pod = {'metadata': {'uid': 'pod-original'}, 'spec': {
            'nodeName': 'node-a', 'volumes': [{'persistentVolumeClaim': {'claimName': 'shared'}}]},
            'status': {'containerStatuses': [{'ready': True,
                'imageID': experiment.args.image, 'containerID': 'container-original'}]}}
        volume = {'metadata': {'uid': 'pvc-original'}}
        for bad, expected in (('node', 'unexpected node'), ('pvc', 'PVC identity'),
                              ('image', 'imageID'), ('binary', 'binary differs')):
            current = json.loads(json.dumps(pod))
            current_volume = json.loads(json.dumps(volume))
            checksum = 'b' * 64
            if bad == 'node':
                current['spec']['nodeName'] = 'node-b'
            if bad == 'pvc':
                current_volume['metadata']['uid'] = 'another-volume'
            if bad == 'image':
                current['status']['containerStatuses'][0]['imageID'] = 'old-image'
            if bad == 'binary':
                checksum = 'f' * 64
            with patch.object(experiment, 'get', side_effect=lambda kind, *_:
                              current if kind == 'pod' else current_volume), \
                    patch.object(experiment, 'exec_in', return_value=SimpleNamespace(
                        stdout=checksum + ' /usr/bin/app-cli\n')):
                with self.assertRaisesRegex(RuntimeError, expected):
                    experiment.wait_ready('fs-a', 'node-a', 'shared')

    def test_old_source_commit_or_digest_is_rejected_before_first_cluster_api(self):
        with tempfile.TemporaryDirectory() as directory:
            for key, wrong in (('source_commit', 'e' * 40), ('source_digest', 'f' * 64)):
                report = Path(directory) / (key + '-report.json')
                receipt_path = Path(directory) / (key + '-receipt.json')
                experiment, controlled = self.make_experiment(
                    report=str(report), build_receipt=str(receipt_path))
                receipt = {'schema_version': 1, 'image': experiment.args.image,
                           'app_cli_sha256': 'b' * 64,
                           'source_commit': CURRENT_SOURCE['origin_head'],
                           'source_digest': CURRENT_SOURCE['worktree_sha256']}
                receipt[key] = wrong
                receipt_path.write_text(json.dumps(receipt))
                with patch.object(TOOL, 'source_identity', return_value=CURRENT_SOURCE):
                    self.assertEqual(experiment.run(), 1)
                self.assertEqual(controlled.calls, [])
                value = json.loads(report.read_text())
                self.assertFalse(value['success'])
                self.assertIn('does not match current frozen source', value['error'])

    def test_source_change_during_run_fails_after_precise_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'report.json')
            receipt_path = Path(directory) / 'receipt.json'
            experiment, controlled = self.make_experiment(report=path,
                                        build_receipt=str(receipt_path))
            receipt_path.write_text(json.dumps({
                'schema_version': 1, 'image': experiment.args.image, 'app_cli_sha256': 'b' * 64,
                'source_commit': CURRENT_SOURCE['origin_head'],
                'source_digest': CURRENT_SOURCE['worktree_sha256']}))

            def exercise(_):
                self.captured_resources(experiment)
                experiment.report['checks'] = [
                    {'name': name, 'ok': True} for name in TOOL.CEPHFS_STEPS]

            changed = dict(CURRENT_SOURCE, worktree_sha256='f' * 64)
            with patch.object(TOOL, 'source_identity', side_effect=[CURRENT_SOURCE, changed]), \
                    patch.object(experiment, 'run_cephfs', side_effect=exercise):
                self.assertEqual(experiment.run(), 1)
            result = json.loads(Path(path).read_text())
            self.assertTrue(result['cleanup_ok'])
            self.assertFalse(result['success'])
            self.assertIn('source_error', result)
            self.assertEqual(result['retained_volumes'][0]['uid'], 'pvc-original')
            self.assertEqual(result['deleted_pods'][0]['uid'], 'pod-original')
            self.assertTrue(result['deleted_pods'][0]['confirmed_absent'])
            source_checks = [c for c in result['checks'] if c['name'] == TOOL.SOURCE_STEP]
            self.assertEqual(len(source_checks), 1)
            self.assertFalse(source_checks[0]['ok'])

    def test_empty_current_source_cannot_self_attest_its_receipt(self):
        for current in ({}, {'origin_head': '', 'worktree_sha256': ''}):
            with self.assertRaisesRegex(ValueError, 'identity is unavailable'):
                TOOL.require_build_source({}, current)


if __name__ == '__main__':
    unittest.main()
