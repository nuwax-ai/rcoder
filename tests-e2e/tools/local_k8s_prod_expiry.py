#!/usr/bin/env python3
"""Continue only the remaining real R1 expiry case after explicit Root recovery.

Historical reports remain untouched. This run independently verifies recovery B,
the original failed gate, a new live hot holder, real 30s rejection, no late
admission and final compute Stops retaining all PVCs. No Cargo or purge.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import time
import urllib.parse
import uuid

from local_k8s_core import digest, require, safe
from local_k8s_prod import validate_busy
from local_k8s_prod_execute import Execute, RUNTIME_IMAGE, operation_identity


def retained_gate_failure(report, observation, app, namespace, run_id, lifecycle):
    require(report.get('success') is False and report.get('app_id')==app and report.get('namespace')==namespace
            and report.get('run_id')==run_id,'original failed execution report belongs to another scene')
    names={row.get('name') for row in report.get('checks',[]) if row.get('passed') is True}
    require({'cold actual operation business HTTP worker and PVC data complete','real Lease R2 busy on primary',
             'real Lease R2 busy on secondary','R1 released real holder resumes same original request'}<=names,
            'original report lacks the actually completed prerequisite stages')
    pending=report.get('pending_requests',[])
    require(not any(row.get('method')=='POST' and '/restart' in row.get('route','')
                    and row.get('request',{}).get('request_id','').startswith('expire-') for row in pending),
            'the old expiry restart was already dispatched; do not issue another write')
    failed=[row for row in pending if row.get('method')=='POST' and row.get('route')=='/api/v1/userapp/'+app+'/start'
            and '/gated/r1-expire-' in row.get('request',{}).get('url','')]
    require(len(failed)==1 and failed[0]['request'].get('lifecycle_id')==lifecycle,'original failed gate request is not unique')
    body=observation.get('management',{}).get('body',{})
    runtime=body.get('data',{}).get('operation',{})
    durable=observation.get('holder',{}).get('body',{}).get('data',{})
    rid=failed[0]['request']['request_id']
    require(observation.get('app')==app and runtime.get('persisted') is True and runtime.get('phase')=='failed'
            and runtime.get('deploy_stage')=='failed' and runtime.get('operation_id')
            and runtime.get('deployment_generation_id')==runtime['operation_id']
            and 'operation timed out' in runtime.get('error','').lower(), 'original runtime gate timeout has no verified Failed evidence')
    require(durable.get('operation_id')==runtime['operation_id'] and durable.get('request_id')==rid
            and durable.get('app_id')==app and durable.get('lifecycle_id')==lifecycle and durable.get('state')=='Failed'
            and durable.get('scope')=='Prod','original durable gate operation did not fail under its own identity')
    require(observation.get('gate',{}).get('body')=={'entered':True,'released':False},'original gate timeout was not an entered unreleased download')
    return durable


def recovery_identity(receipt, app, lifecycle, artifact):
    request=receipt.get('request',{});reply=receipt.get('reply',{});record=receipt.get('original_operation',{})
    require(receipt.get('dispatched') is True and receipt.get('status')==200 and reply.get('success') is True
            and reply.get('code')=='0000' and request.get('lifecycle_id')==lifecycle
            and request.get('release_id')==artifact['release_id'] and request.get('sha256')==artifact['sha256'],
            'explicit real recovery B response or immutable artifact differs')
    operation_identity(record,app,lifecycle,request.get('request_id'),'StartDeployment')
    require(record.get('state')=='Succeeded' and record.get('operation_id')==reply.get('operation_id')
            and record['operation_id']==reply.get('data',{}).get('operation_id'),'explicit recovery did not complete its original operation')
    return record


class Expiry(Execute):
    report_prefix='local-k8s-prod-expiry-'

    def controller_proof(self):
        return {**super().controller_proof(),'expiry_harness_sha256':digest(__file__)}

    def retained(self,path,label):
        path=Path(path).resolve();require(path.is_file() and path.is_relative_to(self.root),label+' must be retained in this owned run')
        return json.loads(path.read_text()),{'path':str(path),'sha256':digest(path)}

    def run_remaining(self, prior_report, failed_observation, recovery_receipt, artifact_url):
        self.report['scope']='real local K8s continuation: recovered B readiness and R1 real30s expiry only'
        self.ownership();before=self.controller_proof();self.report['inputs_before']=before
        prior,prior_ref=self.retained(prior_report,'original failed execution')
        observation,observation_ref=self.retained(failed_observation,'original failed gate observation')
        recovery,recovery_ref=self.retained(recovery_receipt,'explicit recovery B')
        failed=retained_gate_failure(prior,observation,self.app,self.namespace,self.identity['run_id'],self.lifecycle_id)
        restored=recovery_identity(recovery,self.app,self.lifecycle_id,self.artifacts['B'])
        require(restored['operation_id']!=failed['operation_id'] and restored['request_id']!=failed['request_id'],'recovery replayed the old failure')
        self.report['retained_prior_stages']={'report':prior_ref,'failed_observation':observation_ref,'explicit_recovery':recovery_ref,
            'original_checks':[{key:row.get(key) for key in ['name','passed']} for row in prior['checks']],
            'interpretation':'R1 prior Succeeded proves rollout instruction ACK; its old worker observation does not prove the successor Pod ready'}
        old=self.by_request(failed['request_id']);now=self.by_request(restored['request_id'])
        require(old and old.get('operation_id')==failed['operation_id'] and old.get('state')=='Failed','original failed gate was overwritten')
        require(now and now.get('operation_id')==restored['operation_id'] and now.get('state')=='Succeeded','original explicit recovery is not completed')
        builder=self.physical(True)
        require(builder['sts']['uid']==self.original['sts']['uid'] and builder['pod']['uid']==self.original['pod']['uid'],'original Stage1 builder was replaced')
        original=self.poll('explicit recovery current Prod Pod',self.prod_physical,lambda value:bool(value['pod']),180)
        self.prod_original=original
        # The old full runner created this root-level sentinel before the failure.
        prefix='local-k8s-prod-execute-'+self.app+'-';name=Path(prior_ref['path']).name
        require(name.startswith(prefix) and name.endswith('.json'),'prior execution filename cannot identify its owned sentinel')
        suffix=name[len(prefix):-5]
        require(suffix and all(char in 'abcdefghijklmnopqrstuvwxyz0123456789-' for char in suffix),'prior sentinel suffix is not literal owned data')
        self.sentinel_name='.local-k8s-prod-sentinel-'+suffix
        ready=self.business_ready(original,'B',180)
        data=self.workspace_and_worker(original)
        require(data['workers'][0]['lock_sha256']==self.artifacts['B']['release_lock_sha256'],'recovered actual worker does not use the original B lock')
        self.check('explicit recovery B has actual current Pod management HTTP and original PVC sentinel',True,{'operation':restored,'failed_original':old,'ready':ready,'data':data})
        self.prepare_artifacts(RUNTIME_IMAGE,False,artifact_url)
        workers=ThreadPoolExecutor(max_workers=1)
        try:
            gate='r1-expire-resume-'+uuid.uuid4().hex[:12]
            future,rid,holder,lease=self.holder(workers,gate)
            require(self.prod_physical()['pod']['uid']==original['pod']['uid'] and self.content(original,self.artifacts['B']['marker']),
                    'live hot holder did not preserve the pinned recovered business')
            denied='expire-'+uuid.uuid4().hex;started=time.monotonic()
            status,body=self.call('POST','/api/v1/userapp/'+self.app+'/restart',{'request_id':denied,'lifecycle_id':self.lifecycle_id},True,45)
            elapsed=time.monotonic()-started
            require(status==200 and 29<=elapsed<=38,'R1 must exhaust its actual configured default30s admission budget')
            validate_busy(body,holder['operation_id'])
            require(self.by_request(denied,True) is None and not future.done(),'expired R1 admitted or displaced the live original holder')
            self.check('R1 real30s rejection retains original live holder and no admission',True,{'response':body,'elapsed':elapsed,'holder':holder,'lease':lease,'rejected_request_id':denied})
            self.release(gate);completed=future.result(timeout=self.remaining(180))
            require(completed.get('operation_id')==holder['operation_id'] and completed.get('state')=='Succeeded','released original holder was replaced or failed')
            self.business_ready(original,'B',180)
            end=min(self.deadline,time.monotonic()+5);samples=0
            while time.monotonic()<end:
                require(self.by_request(denied,True) is None and self.prod_physical()['pod']['uid']==original['pod']['uid'],'expired R1 executed late after release')
                samples+=1;time.sleep(min(0.5,max(0,end-time.monotonic())))
            require(samples>=2,'no late admission observation was empty')
            self.check('released same holder succeeds without new Pod or late rejected restart',True,{'operation':completed,'pod_uid':original['pod']['uid'],'rejected_request_id':denied,'observation_seconds':5,'samples':samples})
        finally:workers.shutdown(wait=False,cancel_futures=False)
        prod_stop=self.compute_stop('prod');prod=self.poll('final retained Prod compute Stop',lambda:self.prod_physical(False),bool,180)
        dev_stop=self.compute_stop('dev');builder=self.poll('final retained builder compute Stop',lambda:self.physical(False),bool,180)
        after=self.controller_proof();require(before==after,'current controller or harness changed during the continuation')
        require(digest(prior_ref['path'])==prior_ref['sha256'] and digest(observation_ref['path'])==observation_ref['sha256']
                and digest(recovery_ref['path'])==recovery_ref['sha256'],'original historical evidence bytes changed')
        self.check('final compute Stops preserve original Prod and builder PVCs',True,{'prod_stop':prod_stop,'dev_stop':dev_stop,'prod':prod,'builder':builder})
        self.report.update(success=True,inputs_after=after,continuation_harness_sha256=digest(__file__));self.persist()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['root','build-report','controller-revision-proof','prior-report','failed-observation','recovery-receipt']:parser.add_argument('--'+name,required=True,type=Path)
    for name in ['primary','peer-url','app','artifact-url']:parser.add_argument('--'+name,required=True)
    parser.add_argument('--report-suffix');parser.add_argument('--budget-seconds',type=int,default=1200)
    parser.add_argument('--authorize-owned-mutations',action='store_true');args=parser.parse_args()
    require(args.authorize_owned_mutations,'Root must authorize this independent real continuation')
    require(600<=args.budget_seconds<=1800,'bounded continuation budget required');harness=None
    try:
        harness=Expiry(args.root,args.primary,args.peer_url,args.app,args.budget_seconds,args.build_report,args.report_suffix,args.controller_revision_proof)
        harness.run_remaining(args.prior_report,args.failed_observation,args.recovery_receipt,args.artifact_url)
    except Exception as error:
        if harness:harness.failure_evidence(error)
        print(json.dumps({'success':False,'error':safe(str(error))}),flush=True);return 1
    print(json.dumps({'success':True,'report':str(harness.report_path)}),flush=True);return 0


if __name__=='__main__':raise SystemExit(main())
