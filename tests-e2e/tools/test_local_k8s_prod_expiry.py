"""Remaining-stage receipt contracts; no HTTP/K8s/Cargo operations."""
import copy
import unittest

import local_k8s_prod_expiry as expiry


class RemainingTests(unittest.TestCase):
    def prior(self):
        names=['cold actual operation business HTTP worker and PVC data complete','real Lease R2 busy on primary',
               'real Lease R2 busy on secondary','R1 released real holder resumes same original request']
        request={'request_id':'hold-original','lifecycle_id':'original-life','url':'http://owned.svc/gated/r1-expire-old/B.zip'}
        report={'success':False,'app_id':'ownedapp','namespace':'rcoder-owned','run_id':'a'*32,
                'checks':[{'name':name,'passed':True} for name in names],
                'pending_requests':[{'method':'POST','route':'/api/v1/userapp/ownedapp/start','request':request}]}
        durable={'operation_id':'original-failed','request_id':'hold-original','app_id':'ownedapp','lifecycle_id':'original-life',
                 'scope':'Prod','state':'Failed'}
        runtime={'operation_id':'original-failed','deployment_generation_id':'original-failed','phase':'failed',
                 'deploy_stage':'failed','persisted':True,'error':'prepare: operation timed out'}
        observation={'app':'ownedapp','management':{'body':{'data':{'operation':runtime}}},
                     'holder':{'body':{'data':durable}},'gate':{'body':{'entered':True,'released':False}}}
        return report,observation

    def validate(self,report,observation):
        return expiry.retained_gate_failure(report,observation,'ownedapp','rcoder-owned','a'*32,'original-life')

    def test_actual_failure_requires_matching_durable_and_runtime_originals(self):
        report,observation=self.prior()
        self.assertEqual(self.validate(report,observation)['operation_id'],'original-failed')
        for key,value in [('state','Succeeded'),('operation_id','other'),('request_id','later')]:
            changed=copy.deepcopy(observation);changed['holder']['body']['data'][key]=value
            with self.subTest(key=key),self.assertRaises(RuntimeError):self.validate(report,changed)
        changed=copy.deepcopy(observation);changed['management']['body']['data']['operation']['persisted']=False
        with self.assertRaises(RuntimeError):self.validate(report,changed)
        changed=copy.deepcopy(observation);changed['gate']['body']['released']=True
        with self.assertRaises(RuntimeError):self.validate(report,changed)

    def test_expiry_cannot_continue_if_old_restart_write_was_already_dispatched(self):
        report,observation=self.prior()
        report['pending_requests'].append({'method':'POST','route':'/api/v1/userapp/ownedapp/restart',
                                           'request':{'request_id':'expire-already-dispatched'}})
        with self.assertRaises(RuntimeError):self.validate(report,observation)

    def test_recovery_ack_may_be_starting_but_original_operation_must_succeed(self):
        artifact={'release_id':'original-B','sha256':'b'*64}
        request={'request_id':'explicit-recovery','lifecycle_id':'original-life',**artifact}
        record={'operation_id':'new-recovery','app_id':'ownedapp','lifecycle_id':'original-life','request_id':'explicit-recovery',
                'scope':'Prod','kind':'StartDeployment','state':'Succeeded'}
        receipt={'dispatched':True,'status':200,'request':request,'original_operation':record,
                 'reply':{'success':True,'code':'0000','operation_id':'new-recovery','data':{'operation_id':'new-recovery','phase':'Starting','pod_ip':None}}}
        self.assertEqual(expiry.recovery_identity(receipt,'ownedapp','original-life',artifact),record)
        for change in [{'state':'Running'},{'operation_id':'old-failure'},{'scope':'Dev'}]:
            changed={**receipt,'original_operation':{**record,**change}}
            with self.subTest(change=change),self.assertRaises(RuntimeError):expiry.recovery_identity(changed,'ownedapp','original-life',artifact)
        with self.assertRaises(RuntimeError):expiry.recovery_identity(receipt,'ownedapp','original-life',{**artifact,'sha256':'c'*64})


if __name__=='__main__':unittest.main()
