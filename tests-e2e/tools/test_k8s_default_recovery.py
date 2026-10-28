import unittest
import json
import subprocess
from types import SimpleNamespace
from unittest.mock import Mock
import k8s_default_recovery as recovery


class RecoveryContractTests(unittest.TestCase):
    def test_multihomed_entry_is_only_allowed_for_the_actual_ssh_test_node(self):
        from k8s_userapp import node_entry_addresses
        interfaces=[{'addr_info':[{'family':'inet','local':'192.0.2.10'},{'family':'inet','local':'198.51.100.10'},{'family':'inet','local':'127.0.0.1'}]}]
        self.assertIn('198.51.100.10',node_entry_addresses({'192.0.2.10'},interfaces))
        self.assertNotIn('198.51.100.10',node_entry_addresses({'203.0.113.10'},interfaces))
        self.assertNotIn('127.0.0.1',node_entry_addresses({'192.0.2.10'},interfaces))

    def test_fault_deletes_only_userapp_tables_in_foreign_key_order(self):
        tables = ['userapps','userapp_operations','userapp_active_operations','userapp_operation_inputs']
        edges = [('userapp_operations','userapps'),('userapp_active_operations','userapp_operations'),('userapp_operation_inputs','userapp_operations')]
        ordered = recovery.deletion_order(tables,edges)
        for child,parent in edges:
            self.assertLess(ordered.index(child),ordered.index(parent))
        with self.assertRaises(AssertionError):
            recovery.deletion_order(['userapps','unrelated'],[])
        with self.assertRaises(AssertionError):
            recovery.deletion_order(['userapps'],[('outside','userapps')])
        with self.assertRaises(AssertionError):
            recovery.deletion_order(['userapps','userapp_operations'],[('userapps','userapp_operations'),('userapp_operations','userapps')])

    def test_fixture_sessions_follow_binding_identity_and_only_declared_deferred_cycle_is_allowed(self):
        tables = ['userapps', 'userapp_agent_bindings', 'userapp_agent_sessions']
        edges = [('userapp_agent_sessions', 'userapp_agent_bindings', False),
                 ('userapp_agent_bindings', 'userapp_agent_sessions', True)]
        ordered = recovery.deletion_order(tables, edges)
        self.assertLess(ordered.index('userapp_agent_sessions'), ordered.index('userapp_agent_bindings'))
        statements = recovery.fixture_deletions(ordered, 'e2e-k8s-'+'a'*14)
        session = next(s for s in statements if s.startswith('DELETE FROM userapp_agent_sessions'))
        self.assertIn("WHERE binding_generation IN (SELECT binding_generation FROM userapp_agent_bindings WHERE app_id='e2e-k8s-"+'a'*14+"')", session)
        with self.assertRaises(AssertionError):
            recovery.deletion_order(tables, [(c,p,False) for c,p,_ in edges])
        with self.assertRaises(AssertionError):
            recovery.deletion_order(tables, edges+[('outside','userapps',True)])
        with self.assertRaises(AssertionError):
            recovery.fixture_deletions(['userapps'], '51')

    def test_metadata_fault_uses_json_argument_and_observes_unknown_ack_before_retry(self):
        app='e2e-k8s-'+'c'*14
        original={'metadata':{'name':'rcoder-app-builder-'+app+'-workspace','uid':'original','resourceVersion':'1',
            'labels':{'app.kubernetes.io/managed-by':'rcoder-runtime'},
            'annotations':{'rcoder.io/application-id':app,'rcoder.io/lifecycle-id':'life'}}}
        patched=json.loads(json.dumps(original));patched['metadata']['annotations']={}
        run=SimpleNamespace(app=app,lifecycle_id='life',args=SimpleNamespace(namespace='rcoder-e2e-unit'),kube=Mock(side_effect=[json.dumps(original),json.dumps(patched)]))
        recovery.strip_legacy_identity(run,original['metadata']['name'],'original')
        args=run.kube.call_args_list[1].args
        self.assertIn('--patch',args);self.assertNotIn('--patch-file=-',args)
        body=json.loads(args[args.index('--patch')+1])
        self.assertEqual(body[0],{'op':'test','path':'/metadata/uid','value':'original'})
        run.kube=Mock(side_effect=[json.dumps(original),subprocess.CalledProcessError(1,['kubectl']),json.dumps(patched)])
        recovery.strip_legacy_identity(run,original['metadata']['name'],'original')
        self.assertEqual(sum(call.args[0]=='patch' for call in run.kube.call_args_list),1)
        run.kube=Mock(return_value=json.dumps(original))
        with self.assertRaises(AssertionError):
            recovery.strip_legacy_identity(run,original['metadata']['name'],'replaced')
        self.assertEqual(run.kube.call_count,1)

    def test_database_fault_is_refused_for_shared_namespace_or_non_fixture_app(self):
        run = SimpleNamespace(args=SimpleNamespace(namespace='nuwax-k8s-test'),app='51',kube=Mock())
        with self.assertRaises(AssertionError):
            recovery.sql(run,'DELETE FROM userapps')
        run.kube.assert_not_called()

    def test_developer_admission_is_not_mistaken_for_successful_startup(self):
        run = SimpleNamespace(app='e2e-k8s-'+'a'*14,user='fixture',entries=['http://fixture'],
            request=Mock(return_value=(200,{'success':True,'data':{'task_id':'original-task'}})),
            api=Mock(),poll=Mock(return_value={'task_id':'original-task','status':'failed'}))
        with self.assertRaisesRegex(AssertionError,'task failed'):
            recovery.dev_restart(run)
        self.assertEqual(run.request.call_count,1)
        run.poll.return_value={'task_id':'original-task','status':'completed'}
        result=recovery.dev_restart(run)
        self.assertEqual(result['task_id'],'original-task')

    def test_unknown_or_failed_request_is_never_blindly_replayed(self):
        run=SimpleNamespace(app='e2e-k8s-'+'b'*14,user='fixture',entries=['http://fixture'],
            request=Mock(return_value=(500,{'success':False,'code':'ERR_BACKEND_ERROR'})))
        with self.assertRaisesRegex(AssertionError,'restart failed'):
            recovery.dev_restart(run)
        self.assertEqual(run.request.call_count,1)

if __name__=='__main__':
    unittest.main()
