"""仅 owner continuation 协议测试；不连接 K8s，不派发信号。"""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

import local_k8s_owner_fault as owner


class OwnerContinuationTests(unittest.TestCase):
    def test_exact_wrapped_argv_preserves_rest_and_requires_native_executable_identity(self):
        def require(ok,message,**evidence):owner.require(ok,message)
        namespace={'require':require};exec(owner.CANONICAL_OWNER_CODE,namespace)
        canonical=namespace['canonical_owner_argv'];native=[8,12345]
        raw=['/usr/local/bin/app-cli','app-cli','serve','--workspace','/home/user/ownedapp']
        self.assertEqual(canonical(raw,native,native),['/usr/local/bin/app-cli','serve','--workspace','/home/user/ownedapp'])
        self.assertEqual(raw,['/usr/local/bin/app-cli','app-cli','serve','--workspace','/home/user/ownedapp'])
        for normal in [['app-cli','serve','--workspace','/home/user/ownedapp'],['/usr/local/bin/app-cli','serve','--control-only','--workspace','/home/user/ownedapp']]:
            self.assertEqual(canonical(normal,native,native),normal)
        for argv in [['/other/app-cli','app-cli','serve','--workspace','/home/user/ownedapp'],
                     ['/usr/local/bin/app-cli','unknown','serve','--workspace','/home/user/ownedapp'],
                     ['/usr/local/bin/app-cli','app-cli','app-cli','serve','--workspace','/home/user/ownedapp'],
                     ['/usr/local/bin/app-cli','app-cli','run','--workspace','/home/user/ownedapp']]:
            with self.subTest(argv=argv),self.assertRaises(owner.business.ContractFailure):canonical(argv,native,native)
        with self.assertRaises(owner.business.ContractFailure):canonical(raw,[8,99999],native)

    def test_adapter_keeps_raw_argv_revalidation_all_lock_guards_and_fails_changed_source(self):
        code=owner.owner_code(owner.business.OWNER_IDENTITY_CODE)
        compile(owner.POD_PROGRAM,'actual-owner-fault-pod','exec')
        self.assertIn("'raw_argv':raw_argv",code)
        self.assertIn("if v]==raw_argv,",code)
        for contract in ["lock_evidence(root/'owner.lock',pid,proc)","lock_evidence(root/'work'/generation/'generation.lock',pid,proc)",
                         "receipt.get('process_epoch')==epoch","persisted==kernel","validate_captured_owner"]:
            self.assertIn(contract,code)
        with self.assertRaises(owner.business.ContractFailure):owner.owner_code(owner.business.OWNER_IDENTITY_CODE.replace("'start_time':fields[19]","'start_time':fields[18]"))

    def report(self):
        app='ownedapp';run='a'*32
        guard={'owner_guard':'captured process is not a live serve owner','pid':180,
               'argv':['/usr/local/bin/app-cli','app-cli','serve','--workspace','/home/user/'+app]}
        tasks=[{'id':task,'kind':kind,'route':'/api/v1/userapp/tasks/'+task+'?app_id='+app,
            'sse_route':'/api/v1/userapp/tasks/'+task+'/logs/stream?app_id='+app+'&from_seq=0'}
            for task,kind in [('cancelled-original','dev_restart'),('completed-source-b','dev_start')]]
        return {'success':False,'app_id':app,'run_id':run,'namespace':'rcoder-owned','namespace_uid':'actual-ns',
            'error':'wrapper diagnostic\nRuntimeError: '+json.dumps(guard)+'\ncommand exited 1','tasks':tasks,
            'checks':[{'name':name,'passed':True} for name in ['Stop during actual build cancels original task without late startup',
                'start actually builds current Source and serves HTTP']],
            'resume_gate_origin':{'physical':{'pod':{'uid':'original-pod'}}}}

    def test_continuation_only_accepts_exact_pre_signal_guard_and_original_completed_stages(self):
        original=self.report();tasks,physical=owner.origin_identity(original,'ownedapp','a'*32,'rcoder-owned')
        self.assertEqual(tasks[1]['id'],'completed-source-b');self.assertEqual(physical['pod']['uid'],'original-pod')
        for change in [{'success':True},{'app_id':'other'},{'namespace_uid':None},{'captured_owner_before_kill':{}},
                       {'error':'RuntimeError: {"owner_guard":"kernel flock unverified"}'}]:
            with self.subTest(change=change),self.assertRaises(owner.business.ContractFailure):
                owner.origin_identity({**original,**change},'ownedapp','a'*32,'rcoder-owned')
        for kind in ('failed_stage','another_app_query','missing_task'):
            changed=copy.deepcopy(original)
            if kind=='failed_stage':changed['checks'][0]['passed']=False
            elif kind=='another_app_query':changed['tasks'][0]['route']='//other.invalid/task'
            else:changed['tasks'].pop()
            with self.subTest(kind=kind),self.assertRaises(owner.business.ContractFailure):owner.origin_identity(changed,'ownedapp','a'*32,'rcoder-owned')

    def test_new_owner_report_never_overwrites_origin_or_replays_existing_fault_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'kubeconfig';config.write_text('opaque fixture')
            (root/'identity.json').write_text(json.dumps({'root':str(root),'run_id':'a'*32,'namespace':'rcoder-owned','context':'orbstack',
                'cluster_api':'https://127.0.0.1:26443','kubeconfig':str(config),'apps':['ownedapp']}))
            prior=root/'local-k8s-business-ownedapp-resumed2.json';prior.write_text(json.dumps(self.report()));original=prior.read_bytes()
            harness=owner.OwnerFaultHarness(root,'http://127.0.0.1:18297','ownedapp',prior)
            self.assertEqual(harness.report_path.name,'local-k8s-owner-fault-ownedapp.json')
            self.assertEqual(prior.read_bytes(),original)
            with self.assertRaises(owner.business.ContractFailure):owner.OwnerFaultHarness(root,'http://127.0.0.1:18297','ownedapp',prior)

    def proc_fixture(self, root, denied):
        binary=root/'app-cli';binary.write_text('trusted immutable fixture');other=root/'python3';other.write_text('another native executable')
        for pid,native in [(10,binary),(11,other),(12,binary),(177,other)]:
            path=root/str(pid);path.mkdir();(path/'exe').symlink_to(native)
            fields=['S']+['0']*19;fields[19]=str(pid*100)
            (path/'stat').write_text(str(pid)+' (native) '+' '.join(fields))
            (path/'cmdline').write_bytes(b'fixture\0owned\0')
            (path/'environ').write_bytes(b'PROJECT_ID=ownedapp\0APP_CLI_RUNTIME_WORKSPACE=/home/user/ownedapp\0APP_CLI_STATE_ROOT=/home/user/ownedapp/state/ownedapp\0')
        class Proc:
            def __init__(self,path):self.path=path
            @property
            def name(self):return self.path.name
            def __truediv__(self,value):return Proc(self.path/value)
            def iterdir(self):return [Proc(value) for value in self.path.iterdir()]
            def stat(self):
                if (self.path.parent.name,self.path.name) in denied:raise PermissionError(13,'Permission denied',str(self.path))
                return self.path.stat()
            def read_text(self):return self.path.read_text()
            def read_bytes(self):
                if (self.path.parent.name,self.path.name) in denied:raise PermissionError(13,'Permission denied',str(self.path))
                return self.path.read_bytes()
        def require(ok,message,**evidence):owner.require(ok,message)
        namespace={'Path':Path,'require':require,'os':Mock(),'signal':Mock(),'validate_captured_owner':Mock()}
        exec(owner.business.PROCESS_FAULT_CODE,namespace);exec(owner.PROCESS_ENUMERATION_CODE,namespace)
        capture={'pid':10,'authority':{'application_id':'ownedapp','workspace':'/home/user/ownedapp','state_root':'/home/user/ownedapp/state/ownedapp'}}
        return namespace,capture,Proc(root),binary

    def test_unknown_postgres_like_proc_permission_is_recorded_without_claiming_exit_or_signal(self):
        with tempfile.TemporaryDirectory() as temporary:
            namespace,capture,proc,binary=self.proc_fixture(Path(temporary),{('177','exe')})
            result=namespace['capture_cli_processes'](capture,[{'pid':11}],proc,binary)
            self.assertEqual([value['pid'] for value in result['processes']],[10,12])
            self.assertEqual(result['unknown_inaccessible_candidates'],[{'pid':177,'observation':'permission_denied_unknown','exit_confirmed':False}])
            registered=[value for value in result['registered_observations'] if value['process']['pid']==11]
            self.assertEqual(len(registered),1);self.assertFalse(registered[0]['native_app_cli'])
            namespace['signal'].pidfd_send_signal.assert_not_called()
            namespace['validate_captured_owner'].assert_called_once()

    def test_registered_application_or_native_cli_permission_still_blocks_before_any_signal(self):
        for denied in [{('10','exe')},{('11','exe')},{('12','environ')}]:
            with self.subTest(denied=denied),tempfile.TemporaryDirectory() as temporary:
                namespace,capture,proc,binary=self.proc_fixture(Path(temporary),denied)
                with self.assertRaisesRegex(owner.business.ContractFailure,'identity unreadable'):
                    namespace['capture_cli_processes'](capture,[{'pid':11}],proc,binary)
                namespace['signal'].pidfd_send_signal.assert_not_called()

    def test_second_report_refuses_prior_signal_dispatch_and_preserves_permission_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'kubeconfig';config.write_text('opaque fixture')
            (root/'identity.json').write_text(json.dumps({'root':str(root),'run_id':'a'*32,'namespace':'rcoder-owned','context':'orbstack',
                'cluster_api':'https://127.0.0.1:26443','kubeconfig':str(config),'apps':['ownedapp']}))
            prior=root/'local-k8s-business-ownedapp-resumed2.json';prior.write_text(json.dumps(self.report()))
            first=root/'local-k8s-owner-fault-ownedapp.json'
            data={'success':False,'run_id':'a'*32,'app_id':'ownedapp','namespace_uid':'actual-ns','namespace':'rcoder-owned',
                  'tasks':[],'error':"PermissionError: [Errno 13] /proc/177/exe",'origin':{'path':str(prior),'sha256':owner.digest(prior)}}
            first.write_text(json.dumps({**data,'fault_dispatch':{'result_unknown':True}}))
            with self.assertRaises(owner.business.ContractFailure):owner.OwnerFaultHarness(root,'http://127.0.0.1:18297','ownedapp',prior,report_suffix='second')
            first.write_text(json.dumps(data));original=first.read_bytes()
            harness=owner.OwnerFaultHarness(root,'http://127.0.0.1:18297','ownedapp',prior,report_suffix='second')
            self.assertEqual(harness.report_path.name,'local-k8s-owner-fault-ownedapp-second.json')
            self.assertEqual(first.read_bytes(),original)


if __name__=='__main__':
    unittest.main()
