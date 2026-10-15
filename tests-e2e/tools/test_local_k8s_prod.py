"""No-resource fixture/SSE/payload/proposal tests; never represent real K8s passes."""
import hashlib
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import zipfile

import local_k8s_prod as prod


class ProtocolTests(unittest.TestCase):
    def test_resume_requires_proved_flat_import_without_any_build_dispatch(self):
        record={'success':False,'stage':'build','app_id':'ownedapp','namespace':'rcoder-owned','run_id':'a'*32,
            'builder':{'pod':{'uid':'original'}},'lifecycle_id':'life','tasks':[],'artifacts':{},'timeline':[{'stage':'http_reply','evidence':{
                'method':'POST','route':'/api/v1/userapp/init-project-template','status':200,'body':{'success':True,
                'message':'Project template initialized successfully','workspace_root':'/home/user/ownedapp'}}}]}
        self.assertEqual(prod.validate_import_resume(record,'ownedapp','rcoder-owned','a'*32)[1],'life')
        for changed in [{'success':True},{'lifecycle_id':None},{'app_id':'other'},{'tasks':[{'task_id':'already-dispatched'}]}, {'artifacts':{'A':{}}}, {'timeline':[]}]:
            with self.subTest(changed=changed),self.assertRaises(prod.ContractFailure):prod.validate_import_resume({**record,**changed},'ownedapp','rcoder-owned','a'*32)

    def test_import_uses_its_actual_flat_response_without_accepting_arbitrary_success(self):
        harness=prod.ProdHarness.__new__(prod.ProdHarness)
        harness.url='http://127.0.0.1:18297';harness.app='ownedapp'
        harness.ownership=Mock();harness.event=Mock();harness.remaining=Mock(return_value=5)
        body={'success':True,'message':'Project template initialized successfully','workspace_root':'/home/user/ownedapp'}
        def response(value):return SimpleNamespace(returncode=0,stdout=json.dumps(value).encode()+b'\n200',stderr=b'')
        with patch.object(prod.subprocess,'run',return_value=response(body)):
            self.assertEqual(harness.request('POST','/api/v1/userapp/init-project-template',b'actual multipart'),body)
        for value in [{**body,'workspace_root':'/home/user/other'}, {**body,'success':False}, {'success':True}]:
            with patch.object(prod.subprocess,'run',return_value=response(value)),self.assertRaises(prod.ContractFailure):
                harness.request('POST','/api/v1/userapp/init-project-template',b'actual multipart')
        with patch.object(prod.subprocess,'run',return_value=response(body)),self.assertRaises(prod.ContractFailure):
            harness.request('POST','/api/v1/userapp/build',{'app_id':'ownedapp'})

    def test_http_has_absolute_transfer_budget_and_private_body_stays_on_stdin(self):
        harness = prod.ProdHarness.__new__(prod.ProdHarness)
        harness.url = 'http://127.0.0.1:18297'
        harness.app = 'ownedapp'
        harness.ownership = Mock();harness.event = Mock();harness.remaining = Mock(return_value=5)
        response = SimpleNamespace(returncode=0, stdout=b'{"success":true,"code":"0000","data":{"task_id":"real"}}\n200', stderr=b'')
        payload = {'app_id':'ownedapp', 'pg':{'password':'private-fixture'}}
        with patch.object(prod.subprocess,'run',return_value=response) as run:
            self.assertEqual(harness.request('POST','/api/v1/userapp/build',payload), {'task_id':'real'})
        command = run.call_args.args[0]
        self.assertEqual(command[command.index('--max-time')+1], '5')
        self.assertIn('@-',command);self.assertNotIn('private-fixture',' '.join(command))
        self.assertIn('x-app-id: ownedapp', command, 'real UserApp forwarding requires the original application header')
        self.assertEqual(json.loads(run.call_args.kwargs['input']),payload)
        harness.ownership.assert_called_once()
        response.returncode=28
        with patch.object(prod.subprocess,'run',return_value=response), self.assertRaises(prod.ContractFailure):
            harness.request('POST','/api/v1/userapp/build',payload)
        self.assertTrue(any(call.args[0]=='http_result_unknown' for call in harness.event.call_args_list))

    def test_fixture_has_real_http_and_worker_sources_and_build_commands(self):
        with zipfile.ZipFile(io.BytesIO(prod.fixture_zip('owned-a'))) as archive:
            web = archive.read('web/main.py').decode();worker = archive.read('worker/main.py').decode()
            compile(web, 'fixture-http', 'exec');compile(worker, 'fixture-worker', 'exec')
            self.assertIn('HTTPServer', web);self.assertIn('owned-a', web)
            self.assertIn('while True', worker)
            self.assertIn('startup_probe = "process"', archive.read('worker/project.manifest.toml').decode())
            self.assertIn('artifact.zip', archive.read('web/project.manifest.toml').decode())

    def test_sse_requires_ordered_unique_matching_real_terminal(self):
        expected = {'release_id': 'real-release', 'sha256': 'a'*64}
        terminal = {'event': 'completed', **expected}
        def stream(events):return b''.join(('id: '+str(i)+'\ndata: '+json.dumps(e)+'\n\n').encode() for i,e in events)
        result = prod.completed_sse(stream([(0, {'event': 'log', 'line': 'completed is only text'}), (1, terminal)]), expected)
        self.assertEqual(result['event_count'], 2)
        for events in [[(0, {'event': 'log', 'line': 'completed'})], [(0, terminal),(0, terminal)],
                       [(1, terminal),(0, {'event':'log'})], [(0, {**terminal,'sha256':'b'*64})],
                       [(0, {'event':'failed','error':'real fault'})], [(0, terminal),(1, terminal)]]:
            with self.subTest(events=events), self.assertRaises(prod.ContractFailure):prod.completed_sse(stream(events), expected)

    def test_busy_payload_cannot_be_unknown_legacy_or_current_accepted_request(self):
        body = {'success':False,'code':'ERR_OPERATION_IN_PROGRESS','data': {
            'holder_operation_id':'real-op','holder_kind':'start_deployment','holder_traffic_wake':False,
            'holder_state':'running','holder_step':'hot_execution','retryable':True,'retry_after_seconds':45}}
        prod.validate_busy(body,'real-op')
        for changed in [{'operation_id':'accepted-current'}, {'code':'ERR_CONFLICT'},
                        {'data':{**body['data'],'holder_operation_id':None}}, {'data':{**body['data'],'holder_kind':'deploy'}},
                        {'data':{**body['data'],'retryable':False}}]:
            with self.subTest(changed=changed), self.assertRaises(prod.ContractFailure):prod.validate_busy({**body,**changed},'real-op')

    def test_artifact_proposal_is_owned_read_only_config_data_without_pvc_deletion(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'frozen.zip';path.write_bytes(b'real captured artifact bytes')
            data={version:{'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest()} for version in ('A','B')}
            resources=prod.artifact_proposal('rcoder-owned','a'*32,'ownedapp','registry.invalid/runtime@sha256:'+'b'*64,data)
            self.assertEqual([r['kind'] for r in resources],['ConfigMap','Pod','Service'])
            self.assertTrue(resources[0]['immutable']);self.assertTrue(resources[1]['spec']['containers'][0]['volumeMounts'][0]['readOnly'])
            for resource in resources:
                self.assertEqual(resource['metadata']['namespace'],'rcoder-owned')
                self.assertEqual(resource['metadata']['labels']['rcoder.e2e.owner'],'a'*32)
            compile(prod.ARTIFACT_SERVER,'actual-artifact-http-server','exec')
            for namespace,image in [('rcoder-dev','registry.invalid/runtime@sha256:'+'b'*64),('rcoder-owned','runtime:latest')]:
                with self.assertRaises(prod.ContractFailure):prod.artifact_proposal(namespace,'a'*32,'ownedapp',image,data)
            path.write_bytes(b'changed bytes')
            with self.assertRaises(prod.ContractFailure):prod.artifact_proposal('rcoder-owned','a'*32,'ownedapp','registry.invalid/runtime@sha256:'+'b'*64,data)


if __name__ == '__main__':
    unittest.main()
