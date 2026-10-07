"""业务 harness 的本地协议/故障授权测试；不连接集群，不代替真实 K8s 回归。"""
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock
import zipfile

import local_k8s_business as business


class BusinessProtocolTests(unittest.TestCase):
    def test_fixture_really_builds_scripts_and_migration_really_fails_with_exit_17(self):
        files = business.fixture_files('ownedapp', 'a'*32, 'a')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with zipfile.ZipFile(io.BytesIO(business.source_zip(files))) as archive:archive.extractall(root)
            for name in ('web/main.py', 'web/build.py', 'web/scripts/migrate.py'):
                compile((root/name).read_text(), name, 'exec')
            built = subprocess.run([sys.executable, 'build.py'], cwd=root/'web', capture_output=True, text=True, timeout=10)
            self.assertEqual(built.returncode, 0, built.stderr)
            self.assertEqual((root/'web/builds.log').read_text(), 'actual-build\n')
            with zipfile.ZipFile(root/'web/artifact.zip') as artifact:
                self.assertEqual(artifact.read('scripts/migrate.py').decode(), files['web/scripts/migrate.py'])
                self.assertIn('main.py', artifact.namelist())
                self.assertNotIn('project.manifest.toml', artifact.namelist(), 'assembler supplies the authoritative manifest separately')
            migration = subprocess.run([sys.executable, 'scripts/migrate.py'], cwd=root/'web', capture_output=True, text=True, timeout=10)
            self.assertEqual(migration.returncode, 17)
            self.assertIn('owned-migration-stdout-before-exit-17', migration.stdout)
            self.assertIn('owned-migration-stderr-before-exit-17', migration.stderr)
            self.assertEqual((root/'web/migration-attempts.log').read_text(), 'actual-exit-17\n')
        self.assertIn('shutdown_timeout_seconds=3', files['web/project.manifest.toml'])
        self.assertIn('[devbuild]', files['web/project.manifest.toml'])
        self.assertIn('[devrun]', files['web/project.manifest.toml'])

    def test_task_get_cannot_substitute_another_task_or_scope(self):
        original = {'id': 'original-task', 'app_id': 'ownedapp', 'kind': 'dev_restart', 'status': 'completed'}
        business.task_identity(original, 'ownedapp', 'original-task', 'dev_restart')
        for change in ({'id': 'latest-task'}, {'app_id': 'other'}, {'kind': 'build'}, {'status': 'unknown'}):
            with self.subTest(change=change), self.assertRaises(business.ContractFailure):
                business.task_identity({**original, **change}, 'ownedapp', 'original-task', 'dev_restart')

    def test_actual_sse_requires_unique_terminal_and_startup_not_just_build_ok(self):
        def stream(events):return b''.join(('id: '+str(seq)+'\ndata: '+json.dumps(event)+'\n\n').encode() for seq,event in events)
        startup = {'event': 'service_start_ok', 'service': 'web'}
        terminal = {'event': 'completed', 'release_id': '', 'sha256': ''}
        snapshot = {'status': 'completed'}
        self.assertEqual(len(business.task_sse(stream([(0, startup), (1, terminal)]), snapshot, 'completed')), 2)
        self.assertEqual(len(business.task_sse(stream([(0, {'event':'building','service':'web'}), (1, {'event':'cancelled'})]), {'status':'cancelled'}, 'cancelled')), 2)
        invalid = [[(0, {'event':'build_ok','service':'web'}),(1,terminal)], [(0,startup),(0,terminal)],
                   [(0,startup),(1,terminal),(2,terminal)], [(0,startup),(1,{'event':'failed','error':'actual failure'})],
                   [(0,startup),(1,terminal),(2,{'event':'log','service':'web','line':'late callback'})],
                   [(0,{'event':'service_start_fail','service':'web','error':'actual failure'}),(1,startup),(2,terminal)]]
        for events in invalid:
            with self.subTest(events=events), self.assertRaises(business.ContractFailure):
                business.task_sse(stream(events), snapshot, 'completed')

    def physical(self):
        return {'pod': {'uid':'actual-pod', 'image_status':[{'name':'agent','containerID':'actual-container','imageID':'immutable-image'}]},
                'pvc': {'uid':'actual-pvc','volume_name':'actual-pv'}}

    def test_same_container_recovery_requires_all_real_resource_identities(self):
        original = self.physical();business.same_physical(original, copy.deepcopy(original))
        variants = []
        for group, field in [('pod','uid'), ('pvc','uid'), ('pvc','volume_name')]:
            changed=copy.deepcopy(original);changed[group][field]='replacement';variants.append(changed)
        for field in ('containerID','imageID'):
            changed=copy.deepcopy(original);changed['pod']['image_status'][0][field]='replacement';variants.append(changed)
        for value in variants:
            with self.subTest(value=value), self.assertRaises(business.ContractFailure):business.same_physical(original,value)

    def test_replaced_physical_container_refuses_exec_before_any_fault_or_write(self):
        harness=business.BusinessHarness.__new__(business.BusinessHarness);harness.builder=self.physical()
        harness.physical=Mock(return_value={**self.physical(),'pod':{**self.physical()['pod'],'uid':'other-pod'}})
        harness.kube=Mock(side_effect=AssertionError('must not dispatch exec into a replacement Pod'))
        with self.assertRaises(business.ContractFailure):harness.exec_mode('kill', {'owner':'old'})
        harness.kube.assert_not_called()

    def test_embedded_actual_pod_program_compiles(self):
        compile(business.POD_PROGRAM, 'actual-pod-program', 'exec')

    def test_actual_wrapper_build_argv_keeps_native_executable_cwd_and_process_identity(self):
        def require(ok,message,**evidence):business.require(ok,message)
        namespace={'Path':Path,'require':require};exec(business.GATE_PROCESS_CODE,namespace)
        verify=namespace['verify_build_process'];gate={'pid':1911,'start_time':'6807067'};source=Path('/home/user/ownedapp')
        native=[8,12345]
        for argv in [['python3','build.py'],['/usr/bin/python3','build.py'],['/usr/bin/python3','python3','build.py']]:
            verify(gate,source,source/'web',argv,native,native)
        for argv in [['python3evil','build.py'],['/usr/bin/python3','foreign','build.py'],['/usr/bin/python3','python3','foreign.py'],
                     ['/usr/bin/python3','python3','build.py','extra'],['sh','build.py']]:
            with self.subTest(argv=argv),self.assertRaises(business.ContractFailure):verify(gate,source,source/'web',argv,native,native)
        for changed,cwd,actual in [({'pid':1911,'start_time':None},source/'web',native),
                                  (gate,Path('/home/user/other/web'),native),(gate,source/'web',[8,99999])]:
            with self.assertRaises(business.ContractFailure):verify(changed,source,cwd,['/usr/bin/python3','python3','build.py'],actual,native)

    def test_new_admission_response_loss_keeps_request_unknown_without_fabricating_task(self):
        harness=business.BusinessHarness.__new__(business.BusinessHarness);harness.app='ownedapp'
        harness.report={'tasks':[{'id':'previous-task'}]};harness.persist=Mock()
        harness.request=Mock(side_effect=ConnectionError('new response lost after dispatch'))
        with self.assertRaises(ConnectionError):harness.task('restart')
        pending=harness.report['pending_business_request']
        self.assertEqual(pending['route'],'/api/v1/userapp/dev/restart')
        self.assertTrue(pending['acceptance_unknown']);self.assertFalse(pending['receipt_verified'])
        self.assertNotIn('task_id',pending);self.assertEqual(harness.report['tasks'],[{'id':'previous-task'}])
        self.assertEqual(harness.request.call_count,1)

    def old_report(self):
        captured={'id':'original-build','kind':'build',
                  'route':'/api/v1/userapp/tasks/original-build?app_id=ownedapp',
                  'sse_route':'/api/v1/userapp/tasks/original-build/logs/stream?app_id=ownedapp&from_seq=0'}
        return {'success':False,'app_id':'ownedapp','namespace':'rcoder-owned','run_id':'a'*32,'tasks':[captured],
            'pending_business_request':{'app_id':'ownedapp','action':'build','receipt_verified':True,'acceptance_unknown':False,'task_id':'original-build'},
            'timeline':[{'stage':'http_dispatch','evidence':{'method':'POST','route':'/api/v1/userapp/build'}}],
            'checks':[{'name':'actual builder Pod container PVC identities captured','passed':True,'evidence':self.physical()}]}

    def test_resume_refuses_other_app_unknown_admission_start_and_fault_history(self):
        original=self.old_report();business.resume_report_identity(original,'ownedapp','a'*32,'rcoder-owned')
        values=[]
        for field,value in [('success',True),('app_id','otherapp'),('run_id','b'*32),('namespace','rcoder-dev')]:
            changed=copy.deepcopy(original);changed[field]=value;values.append(changed)
        changed=copy.deepcopy(original);changed['pending_business_request']['acceptance_unknown']=True;values.append(changed)
        changed=copy.deepcopy(original);changed['tasks'][0]['id']='substituted-task';values.append(changed)
        changed=copy.deepcopy(original);changed['timeline'].append({'stage':'http_dispatch','evidence':{'method':'POST','route':'/api/v1/userapp/dev/start'}});values.append(changed)
        changed=copy.deepcopy(original);changed['captured_owner_before_kill']={'pid':12};values.append(changed)
        for value in values:
            with self.subTest(value=value),self.assertRaises(business.ContractFailure):
                business.resume_report_identity(value,'ownedapp','a'*32,'rcoder-owned')

    def test_resume_only_replaces_exact_old_build_file_after_original_failed_get_sse(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);old=self.old_report();before={'source':{'source_inputs_sha256':'b'*64},'binary':'frozen','binary_sha256':'c'*64,
                'host_pid':100,'host_process_start_and_executable':'captured','source_bound_to_build':True,'harness_sha256':'d'*64}
            old['inputs_before']=before;path=root/'old.json';path.write_text(json.dumps(old))
            ns=root/'namespace.json';ns.write_text('{}')
            harness=business.BusinessHarness.__new__(business.BusinessHarness)
            harness.root=root;harness.resume_path=path;harness.resume_namespace_proof=ns;harness.app='ownedapp'
            harness.identity={'run_id':'a'*32};harness.namespace='rcoder-owned';harness.report={};harness.check=Mock()
            harness.physical=Mock(return_value=self.physical());harness.exec_mode=Mock()
            snapshot={'id':'original-build','app_id':'ownedapp','kind':'build','status':'failed',
                'error':'start_file web/project.manifest.toml: invalid Zip archive: Duplicate filename: web/project.manifest.toml'}
            raw=b'id: 0\ndata: {"event":"failed","error":"Duplicate filename"}\n\n'
            harness.request=Mock(side_effect=[snapshot,raw])
            hashes={name:business.hashlib.sha256(text.encode()).hexdigest() for name,text in business.fixture_files('ownedapp','a'*32,'a').items()}
            hashes['web/build.py']=business.PRE_MANIFEST_FIX_BUILD_SHA256
            current={'source_root':'/home/user/ownedapp','source_files_sha256':hashes,'sentinel_matches':True,'starts':[],'migrations':[],
                'builds':['actual-build'],'business':{'status':None}}
            after=copy.deepcopy(current);after['source_files_sha256']['web/build.py']=business.hashlib.sha256(business.BUILD_SOURCE.encode()).hexdigest()
            harness.probe=Mock(side_effect=[current,after])
            harness.resume_source(before)
            self.assertEqual(harness.exec_mode.call_args.args,('write',{'web/build.py':business.BUILD_SOURCE}))
            self.assertEqual(harness.request.call_args_list,[unittest.mock.call('GET',old['tasks'][0]['route']),unittest.mock.call('GET',old['tasks'][0]['sse_route'],raw=True)])
            self.assertEqual(path.read_text(),json.dumps(old))
            self.assertEqual(harness.report['resume_origin']['task']['id'],'original-build')
            # Even a valid failed task cannot authorize overwriting other/current Source input.
            harness.request.side_effect=[snapshot,raw];harness.exec_mode.reset_mock()
            changed=copy.deepcopy(current);changed['source_files_sha256']['web/main.py']='e'*64
            harness.probe.side_effect=[changed]
            with self.assertRaises(business.ContractFailure):harness.resume_source(before)
            harness.exec_mode.assert_not_called()

    def test_resume_has_a_new_report_and_never_overwrites_original_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'kubeconfig';config.write_text('opaque test credential')
            (root/'identity.json').write_text(json.dumps({'root':str(root),'run_id':'a'*32,'namespace':'rcoder-owned',
                'context':'orbstack','cluster_api':'https://127.0.0.1:26443','kubeconfig':str(config),'apps':['ownedapp']}))
            old=root/'local-k8s-business-ownedapp.json';old.write_text(json.dumps(self.old_report()))
            ns=root/'namespace.json';ns.write_text('{}');os.utime(ns,(time_value:=old.stat().st_mtime-1,time_value))
            original=old.read_bytes()
            harness=business.BusinessHarness(root,'http://127.0.0.1:18297','ownedapp',resume_failed_build_report=old,resume_namespace_proof=ns)
            self.assertEqual(harness.report_path.name,'local-k8s-business-ownedapp-resumed.json')
            self.assertEqual(old.read_bytes(),original)
            with self.assertRaises(business.ContractFailure):
                business.BusinessHarness(root,'http://127.0.0.1:18297','ownedapp',resume_failed_build_report=old,resume_namespace_proof=ns)

    def test_resume_does_not_replay_original_failed_task_as_new_build(self):
        harness=business.BusinessHarness.__new__(business.BusinessHarness);harness.app='ownedapp';harness.persist=Mock()
        harness.report={'tasks':[],'resume_origin':{'task':{'id':'original-failed'}}}
        harness.request=Mock(return_value={'task_id':'original-failed'})
        with self.assertRaisesRegex(business.ContractFailure,'new task'):harness.task('build')
        self.assertEqual(harness.report['tasks'],[])

    def gated_report(self):
        old=self.old_report();old['namespace_uid']='actual-namespace';old['resume_origin']={'physical':self.physical()}
        original=old['tasks'][0]
        old['tasks']=[{**original,'id':task,'kind':kind,
            'route':'/api/v1/userapp/tasks/'+task+'?app_id=ownedapp',
            'sse_route':'/api/v1/userapp/tasks/'+task+'/logs/stream?app_id=ownedapp&from_seq=0'}
            for task,kind in [('original-build','build'),('original-start','dev_start'),('original-gate','dev_restart')]]
        names=['real packaged artifact includes migration scripts','start actually builds current Source and serves HTTP',
               'actual migration exit 17 is diagnosed and HTTP remains available']
        old['checks']=[{'name':name,'passed':True,'evidence':{}} for name in names]
        record={'id':'original-gate','app_id':'ownedapp','kind':'dev_restart','status':'running','stage':'building','seq':3}
        envelope=lambda data:{'success':True,'code':'0000','data':data}
        proof={'pod_uid':'actual-pod','task_before':envelope(record),'task_after':envelope({**record,'status':'cancelled','seq':4}),
            'stop':envelope({'app_id':'ownedapp'}),'gate_before':{'gate':'gate-'+'a'*32,'pid':1911,'start_time':'6807067',
            'cwd':'/home/user/ownedapp/web','alive':True}}
        return old,proof

    def test_gated_resume_requires_same_original_running_then_cancelled_stop_and_physical_scope(self):
        original,proof=self.gated_report()
        self.assertEqual(business.gated_stop_report_identity(original,proof,'ownedapp','a'*32,'rcoder-owned')[0]['id'],'original-gate')
        values=[]
        for record,field,value in [('task_after','id','another-task'),('task_after','status','running'),('task_after','seq',3),
                                   ('task_before','status','completed'),('stop','app_id','other-app')]:
            changed=copy.deepcopy(proof);changed[record]['data'][field]=value;values.append(changed)
        changed=copy.deepcopy(proof);changed['pod_uid']='replacement-pod';values.append(changed)
        changed=copy.deepcopy(proof);changed['gate_before']['cwd']='/home/user/other/web';values.append(changed)
        changed=copy.deepcopy(proof);changed['stop']['success']=False;values.append(changed)
        for value in values:
            with self.subTest(proof=value),self.assertRaises(business.ContractFailure):
                business.gated_stop_report_identity(original,value,'ownedapp','a'*32,'rcoder-owned')
        for change in [{'captured_owner_before_kill':{'pid':12}}, {'namespace_uid':None}, {'success':True}, {'app_id':'other'}]:
            with self.subTest(change=change),self.assertRaises(business.ContractFailure):
                business.gated_stop_report_identity({**original,**change},proof,'ownedapp','a'*32,'rcoder-owned')

    def test_cancelled_task_is_not_build_exit_and_changed_gate_never_proves_original_cleanup(self):
        captured={'gate':'original-gate','pid':1911,'start_time':'6807067'}
        self.assertFalse(business.gate_exit_confirmed(captured,{'status':'cancelled','gate':captured,'gate_process_alive':True}))
        self.assertTrue(business.gate_exit_confirmed(captured,{'gate':captured,'gate_process_alive':False}))
        for field,value in [('gate','new-gate'),('pid',5000),('start_time','new-start')]:
            changed={**captured,field:value}
            self.assertFalse(business.gate_exit_confirmed(captured,{'gate':changed,'gate_process_alive':False}))

    def test_gated_resume_uses_new_task_and_report_without_overwriting_prior_source_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'kubeconfig';config.write_text('opaque credential fixture')
            (root/'identity.json').write_text(json.dumps({'root':str(root),'run_id':'a'*32,'namespace':'rcoder-owned','context':'orbstack',
                'cluster_api':'https://127.0.0.1:26443','kubeconfig':str(config),'apps':['ownedapp']}))
            old,proof=self.gated_report();original=root/'local-k8s-business-ownedapp-resumed.json';original.write_text(json.dumps(old))
            stop=root/'actual-stop.json';stop.write_text(json.dumps(proof))
            harness=business.BusinessHarness(root,'http://127.0.0.1:18297','ownedapp',resume_after_gated_stop_report=original,resume_stop_proof=stop)
            self.assertEqual(harness.report_path.name,'local-k8s-business-ownedapp-resumed2.json')
            self.assertEqual(json.loads(original.read_text())['checks'],old['checks'])
            harness.report['resume_gate_origin']={'prior_task_ids':[task['id'] for task in old['tasks']]}
            harness.request=Mock(return_value={'task_id':'original-gate'})
            with self.assertRaisesRegex(business.ContractFailure,'new task'):harness.task('restart')
            self.assertEqual(harness.report['tasks'],[])


class PreciseFaultTests(unittest.TestCase):
    def fixture(self, root):
        binary=root/'app-cli';binary.write_text('immutable executable fixture')
        authority={'workspace':'/home/user/ownedapp','state_root':'/home/user/ownedapp/state/ownedapp','application_id':'ownedapp'}
        for pid in (11,12):
            proc=root/str(pid);proc.mkdir()
            fields=['S']+['0']*19;fields[19]=str(pid*100)
            (proc/'stat').write_text(str(pid)+' (app-cli) '+' '.join(fields))
            (proc/'exe').symlink_to(binary)
            (proc/'cmdline').write_bytes(b'app-cli\0serve\0--workspace\0/home/user/ownedapp\0')
            env={'PROJECT_ID':'ownedapp','APP_CLI_RUNTIME_WORKSPACE':authority['workspace'],'APP_CLI_STATE_ROOT':authority['state_root']}
            (proc/'environ').write_bytes(b'\0'.join((key+'='+value).encode() for key,value in env.items())+b'\0')
        native=Mock();send=Mock();opened=Mock(side_effect=[101,102]);closed=Mock()
        def require(ok,message,**evidence):business.require(ok,message)
        namespace={'Path':Path,'os':SimpleNamespace(pidfd_open=opened,close=closed),
                   'signal':SimpleNamespace(pidfd_send_signal=send,SIGKILL=9), 'require':require,'validate_captured_owner':native}
        exec(business.PROCESS_FAULT_CODE,namespace)
        class Poll:
            def __init__(self):self.fds=set()
            def register(self,fd,flags):self.fds.add(fd)
            def unregister(self,fd):self.fds.remove(fd)
            def poll(self,wait):return [(fd,1) for fd in self.fds]
        namespace['select']=SimpleNamespace(poll=Poll,POLLIN=1)
        owner={'pid':11,'authority':authority}
        captured=[namespace['owned_process'](pid,authority,root) for pid in (11,12)]
        return namespace,owner,captured,send,opened,closed,native

    def test_all_exact_processes_are_verified_before_any_signal_and_exit_is_confirmed(self):
        with tempfile.TemporaryDirectory() as temporary:
            namespace,owner,captured,send,opened,closed,native=self.fixture(Path(temporary))
            result=namespace['signal_captured_processes'](owner,captured,Path(temporary))
            self.assertTrue(result['all_captured_exit_confirmed']);self.assertEqual(result['captured_count'],2)
            self.assertEqual(send.call_args_list,[unittest.mock.call(101,9),unittest.mock.call(102,9)])
            self.assertEqual(native.call_count,2);self.assertEqual(closed.call_count,2)

    def test_pid_reuse_changed_argv_and_foreign_application_never_receive_any_signal(self):
        for kind in ('pid_reuse','changed_argv','foreign_environment'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                root=Path(temporary);namespace,owner,captured,send,opened,closed,native=self.fixture(root)
                if kind=='pid_reuse':(root/'12/stat').write_text((root/'12/stat').read_text().replace('1200','9999'))
                elif kind=='changed_argv':(root/'12/cmdline').write_bytes(b'app-cli\0serve\0--workspace\0/home/user/other\0')
                else:(root/'12/environ').write_bytes((root/'12/environ').read_bytes().replace(b'PROJECT_ID=ownedapp',b'PROJECT_ID=other'))
                with self.assertRaises(business.ContractFailure):namespace['signal_captured_processes'](owner,captured,root)
                send.assert_not_called();self.assertEqual(closed.call_count,2)

    def test_empty_or_duplicate_process_sets_do_not_signal_or_open_raw_pids(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);namespace,owner,captured,send,opened,closed,native=self.fixture(root)
            for values in ([],[captured[0],captured[0]],[captured[1]]):
                with self.assertRaises(business.ContractFailure):namespace['signal_captured_processes'](owner,values,root)
            send.assert_not_called();opened.assert_not_called()

    def test_sent_signal_without_actual_pidfd_exit_evidence_is_not_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);namespace,owner,captured,send,opened,closed,native=self.fixture(root)
            namespace['time']=SimpleNamespace(monotonic=Mock(side_effect=[0,11]))
            with self.assertRaisesRegex(business.ContractFailure,'exit not confirmed'):
                namespace['signal_captured_processes'](owner,captured,root)
            self.assertEqual(send.call_count,2);self.assertEqual(closed.call_count,2)


if __name__=='__main__':
    unittest.main()
