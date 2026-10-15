#!/usr/bin/env python3
"""app-cli runtime recovery v2/v3 fault matrix (plan §11.2 + RV08 rework).

One container chain exercises the unified in-process owner against real
faults: concurrent callers (A), owner TERM/SIGKILL (B), pkill of every
app-cli process (C), Stop-execution admission barrier with zero
persistence (G), a frozen (SIGSTOP) owner with no second-owner takeover
(J), an app-11 style stale Running + RecoveryRequired fixture imported
while the owner is DOWN and read by the next boot (I), damaged-journal
degraded management with HTTP **and** native precise Stop (R3),
same-container restart through the image's real supervisord entrypoint
(E) and same-volume container replacement (D), and a real-identity
advisory migration history, real failure output and execution counters (H).

The container's pid 1 is the image's own supervisord (foreground), with
the fixture programs mounted through /etc/supervisor/conf.d — a docker
restart re-boots services exactly the way the platform image does, with
no post-restart pkill or manual service relaunch.

Real supervisord, real builds, real HTTP content assertions. No LLM, no
RCoder control plane. Leaves the workspace volume intact for inspection;
removes only the containers it created.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

from isolated_docker import require_local_docker_endpoint


# Executed inside the exact fixture container. Keep this as a real subprocess
# argument (no embedded NUL); the safety tests exercise these same guards.
OWNER_IDENTITY_CODE = """import json,os,signal,sys
from pathlib import Path

def require(ok, guard, **evidence):
    if not ok:
        raise RuntimeError(json.dumps({'owner_guard':guard, **evidence},sort_keys=True))

def lock_key(value):
    major,minor,inode=value.split(':')
    return [int(major,16),int(minor,16),int(inode)]

def lock_evidence(path,pid,proc):
    st=path.stat();file_id=[st.st_dev,st.st_ino];keys=[];descriptors=[]
    for fd in (proc/str(pid)/'fd').iterdir():
        try:
            observed=fd.stat()
        except FileNotFoundError:
            continue
        if [observed.st_dev,observed.st_ino]!=file_id:
            continue
        descriptors.append(fd.name)
        for line in (proc/str(pid)/'fdinfo'/fd.name).read_text().splitlines():
            if not line.startswith('lock:'):
                continue
            fields=line.split()[1:]
            require(len(fields)==8 and fields[1:4]==['FLOCK','ADVISORY','WRITE']
                    and fields[4]==str(pid) and fields[6:]==['0','EOF'],
                    'exact descriptor does not hold the exclusive owner lock',
                    path=str(path),pid=pid,fd=fd.name,lock=line)
            key=lock_key(fields[5])
            if key not in keys:
                keys.append(key)
    require(len(keys)==1,'exact owner file has no unique held flock descriptor',
            path=str(path),pid=pid,file_id=file_id,descriptors=descriptors,keys=keys)
    holders=[]
    for line in (proc/'locks').read_text().splitlines():
        fields=line.split()
        if len(fields)==8 and fields[1]=='FLOCK' and lock_key(fields[5])==keys[0]:
            require(fields[2:4]==['ADVISORY','WRITE'] and fields[6:]==['0','EOF'],
                    'owner kernel lock is not exclusive',path=str(path),lock=line)
            holders.append(int(fields[4]))
    require(holders==[pid],'kernel flock holder differs from captured serve owner',
            path=str(path),pid=pid,holders=holders,kernel_key=keys[0],file_id=file_id)
    after=path.stat()
    require([after.st_dev,after.st_ino]==file_id,'stable owner lock file changed during observation',
            path=str(path),expected=file_id,observed=[after.st_dev,after.st_ino])
    # Mounted files can expose a different stat device than the kernel lock.
    # The exact fd proves file identity; fdinfo supplies the kernel device key.
    return {'file_id':file_id,'kernel_key':keys[0],'holder_pid':pid}

def observe_owner(authority,proc=Path('/proc')):
    root=Path(authority['state_root']).resolve(strict=True)
    workspace=Path(authority['workspace']).resolve(strict=True)
    native=authority['native'];kernel=authority['kernel']
    discovery=json.loads((root/'supervisor.json').read_text())
    snapshot=discovery['snapshot'];generation=native.get('generation')
    require(bool(generation),'native owner has no active generation')
    require(discovery['instance']==native['supervisor_id']==snapshot['supervisor_id']
            and snapshot['generation']==generation,
            'native instance or generation changed',expected=native,
            observed_instance=discovery['instance'],observed_snapshot=snapshot)
    binding={'component':'app-cli','resource':str(workspace)}
    require(native['binding']==snapshot['binding']==binding,
            'native binding differs from authorized source root',expected=binding,
            observed=snapshot['binding'])
    persisted=json.loads((root/'identity.json').read_text())
    require(persisted==kernel and kernel['application_id']==authority['application_id']
            and kernel['workspace_id']==authority['application_id']
            and kernel['source_root']==str(workspace),
            'kernel identity differs from live API or authorized workspace',
            expected=kernel,observed=persisted)
    receipt=json.loads((root/'work'/generation/'generation.json').read_text())
    require(receipt['id']==generation and receipt['supervisor']==native['supervisor_id']
            and receipt['phase'] in ('Running','Draining') and receipt.get('worker_pid'),
            'generation receipt does not identify a live unified owner',
            generation=generation,supervisor=receipt.get('supervisor'),
            phase=receipt.get('phase'),worker_pid=receipt.get('worker_pid'))
    require(receipt.get('physical_domain')==authority['physical_domain'],
            'generation physical container or volume differs',
            expected=authority['physical_domain'],observed=receipt.get('physical_domain'))
    pid=int(receipt['worker_pid']);p=proc/str(pid)
    fields=(p/'stat').read_text().rsplit(')',1)[1].split()
    argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]
    require(fields[0]!='Z' and len(argv)>2 and Path(argv[0]).name=='app-cli'
            and argv[1]=='serve','captured process is not a live serve owner',pid=pid,argv=argv)
    indices=[i for i,v in enumerate(argv) if v=='--workspace']
    require(len(indices)==1 and indices[0]+1<len(argv),
            'serve command differs from authorized source root',pid=pid,argv=argv)
    argv_root=Path(argv[indices[0]+1]).resolve(strict=True)
    # The runtime layout folds only the exact .run deployment alias to its
    # canonical parent. Keep the raw argv in the physical identity snapshot.
    require(argv_root==workspace or (argv_root.name=='.run' and argv_root.parent==workspace),
            'serve command differs from authorized source root',pid=pid,argv=argv)
    pid1=(proc/'1'/'stat').read_text().rsplit(')',1)[1].split()[19]
    epoch='pid1:'+(proc/'sys/kernel/random/boot_id').read_text().strip()+':'+pid1
    require(receipt.get('process_epoch')==epoch,'generation process epoch changed',
            expected=receipt.get('process_epoch'),observed=epoch)
    exe=(p/'exe').stat()
    result={'pid':pid,'start_time':fields[19],'argv':argv,
            'executable_id':[exe.st_dev,exe.st_ino],
            'supervisor_id':native['supervisor_id'],'generation':generation,
            'runtime_instance_id':kernel['runtime_instance_id'],'process_epoch':epoch,
            'owner_lock':lock_evidence(root/'owner.lock',pid,proc),
            'generation_lock':lock_evidence(root/'work'/generation/'generation.lock',pid,proc)}
    after=json.loads((root/'supervisor.json').read_text())
    require(after['instance']==native['supervisor_id']
            and after['snapshot']['generation']==generation,
            'native instance or generation changed while capturing physical owner')
    after_fields=(p/'stat').read_text().rsplit(')',1)[1].split()
    require(after_fields[0]!='Z' and after_fields[19]==fields[19]
            and [v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]==argv,
            'process changed while capturing physical owner',pid=pid)
    return result

def validate_captured_owner(captured,proc=Path('/proc')):
    observed=observe_owner(captured['authority'],proc)
    expected={k:v for k,v in captured.items() if k!='authority'}
    require(observed==expected,'captured owner physical identity changed',
            expected=expected,observed=observed)
    return observed

def signal_owner(captured,sig,proc=Path('/proc')):
    require(sig in (9,15),'unsupported controlled fault signal',signal=sig)
    require(hasattr(os,'pidfd_open') and hasattr(signal,'pidfd_send_signal'),
            'pidfd support is required for exact process fault injection')
    fd=os.pidfd_open(captured['pid'],0)
    try:
        validate_captured_owner(captured,proc)
        signal.pidfd_send_signal(fd,sig)
        return {'signal_sent':sig,'pid':captured['pid'],'start_time':captured['start_time']}
    finally:
        os.close(fd)
"""

OWNER_PROCESS_SCRIPT = OWNER_IDENTITY_CODE + """
if __name__=='__main__':
    mode=sys.argv[1];payload=json.loads(sys.argv[2])
    if mode=='capture':
        result=observe_owner(payload);result['authority']=payload
    elif mode=='signal':
        result=signal_owner(payload,int(sys.argv[3]))
    else:
        raise RuntimeError('unsupported owner fixture action')
    print(json.dumps(result))
"""


STOP_CONTROLLER_SCRIPT = OWNER_IDENTITY_CODE + """
import hashlib,http.client,select,socket,time,urllib.error,urllib.request,xmlrpc.client

NONTERMINAL_OPERATION_STATES=('accepted','preparing','stopping','activating','starting')

def runtime_request(method,path,token,body=None,timeout=1):
    data=json.dumps(body).encode() if body is not None else None
    request=urllib.request.Request('http://127.0.0.1:3010'+path,data=data,method=method,
            headers={'content-type':'application/json','x-deploy-token':token})
    try:
        with urllib.request.urlopen(request,timeout=timeout) as response:
            return response.status,json.load(response)
    except urllib.error.HTTPError as error:
        return error.code,json.load(error)

def supervisor_process(socket_path):
    class UnixConnection(http.client.HTTPConnection):
        def connect(self):
            self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            self.sock.settimeout(1)
            self.sock.connect(socket_path)
    class UnixTransport(xmlrpc.client.Transport):
        def make_connection(self,host):
            return UnixConnection(host)
    with xmlrpc.client.ServerProxy('http://localhost/RPC2',transport=UnixTransport()) as client:
        return client.supervisor.getProcessInfo('app-svc-web'),client.supervisor.getPID()

def observe_business(pid,owner,proc=Path('/proc')):
    p=proc/str(pid);fields=(p/'stat').read_text().rsplit(')',1)[1].split()
    argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]
    source=Path(owner['authority']['workspace']).resolve(strict=True)
    cwd=(p/'cwd').resolve(strict=True)
    require(fields[0]!='Z' and fields[1]=='1' and len(argv)==2
            and Path(argv[0]).name.startswith('python3') and argv[1]=='main.py'
            and cwd==source/'web','business process is not the captured Source service',
            pid=pid,state=fields[0],parent=fields[1],argv=argv,cwd=str(cwd))
    executable=(p/'exe').stat()
    return {'pid':pid,'start_time':fields[19],'argv':argv,'cwd':str(cwd),
            'source_root':str(source),'generation':owner['generation'],
            'supervisor_id':owner['supervisor_id'],
            'executable_id':[executable.st_dev,executable.st_ino],
            'source_sha256':hashlib.sha256((cwd/'main.py').read_bytes()).hexdigest()}

def capture_business(owner,proc=Path('/proc')):
    root=Path(owner['authority']['state_root'])
    engine=json.loads((root/'work'/owner['generation']/'supervisord-engine.json').read_text())
    require(engine['generation']==owner['generation']
            and engine['supervisor_id']==owner['supervisor_id']
            and engine['socket']=='/var/run/supervisor.sock',
            'supervisord engine does not belong to captured native generation',engine=engine)
    info,supervisor_pid=supervisor_process(engine['socket'])
    require(info['name']==info['group']=='app-svc-web' and info['state']==20
            and info['statename']=='RUNNING' and info['pid']>1 and supervisor_pid==1,
            'supervisor does not identify a live owned business process',
            info=info,supervisor_pid=supervisor_pid)
    business=observe_business(int(info['pid']),owner,proc)
    return business,{'engine':engine,'process_info':info,'supervisor_pid':supervisor_pid}

def original_stop_evidence(owner,request,code,body,receipt):
    require(code==200 and body.get('success') is True,
            'cannot observe the original Stop operation',status=code,response=body)
    view=body.get('data') or {};disk=receipt['view'];stored=receipt['request']
    keys=('operation_id','kind','request_digest','revision','runtime_instance_id')
    require(view.get('operation_id')==request['operation_id'] and view.get('kind')=='stop'
            and view.get('runtime_instance_id')==owner['runtime_instance_id']
            and bool(view.get('request_digest'))
            and all(view.get(k)==disk.get(k) for k in keys)
            and all(stored.get(k)==v for k,v in request.items()),
            'original Stop receipt identity differs',view=view,stored_view=disk,
            expected_operation_id=request['operation_id'])
    require(view.get('state') in NONTERMINAL_OPERATION_STATES
            and disk.get('state') in NONTERMINAL_OPERATION_STATES,
            'physical Stop window closed or operation state is unsupported',
            operation_id=request['operation_id'],http_state=view.get('state'),
            receipt_state=disk.get('state'))
    return view

def stop_barrier_evidence(owner,business,request,code,body,receipt,ack_path,proc=Path('/proc')):
    view=original_stop_evidence(owner,request,code,body,receipt)
    try:
        ack=ack_path.read_bytes()
    except FileNotFoundError:
        return None
    if not ack:
        return None
    require(ack==str(business['pid']).encode(),
            'physical Stop ACK differs from the captured business PID',
            expected_pid=business['pid'],ack=ack.decode(errors='replace'))
    observed=observe_business(business['pid'],owner,proc)
    require(observed==business,'business physical identity changed before Stop injection',
            expected=business,observed=observed)
    return {'operation':view,'receipt':receipt,'business':observed,
            'ack':{'path':str(ack_path),'bytes':ack.decode(),
                   'mtime_ns':ack_path.stat().st_mtime_ns},'business_alive':True}

def controlled_stop(payload,proc=Path('/proc'),progress=None):
    progress=progress if progress is not None else {}
    owner=payload['owner'];token=payload['deploy_token'];operation_id=payload['operation_id']
    require(operation_id and all(v.isalnum() or v=='-' for v in operation_id),
            'unsafe original operation identifier')
    require(hasattr(os,'pidfd_open') and hasattr(signal,'pidfd_send_signal'),
            'pidfd support is required for exact Stop interruption')
    deadline=time.monotonic()+10
    def remaining():
        left=deadline-time.monotonic()
        message=('captured owner exit proof deadline expired after signal'
                 if 'signal' in progress else 'original Stop ACK window missed; no signal sent')
        require(left>0,message,evidence=progress)
        return left
    def fresh_identity():
        code,body=runtime_request('GET','/v1/runtime/identity',token,timeout=min(1,remaining()))
        require(code==200 and body.get('success') is True
                and body.get('data')==owner['authority']['kernel'],
                'live Kernel API instance changed during original Stop',status=code,response=body)
    fd=os.pidfd_open(owner['pid'],0)
    try:
        progress['stage']='capture_before_stop'
        validate_captured_owner(owner,proc);fresh_identity()
        business,supervised=capture_business(owner,proc)
        progress.update(business=business,supervisor=supervised)
        ack=Path(business['cwd'])/'stop-ack'
        ack.unlink(missing_ok=True)
        require(not ack.exists(),'old Stop ACK was not cleared')
        request={'operation_id':operation_id,
                 'expected_runtime_instance_id':owner['runtime_instance_id'],
                 'expected_revision':payload['revision'],
                 'workspace_id':owner['authority']['application_id'],'kind':'stop',
                 'profile':{'profile':'source',
                            'input':{'workspace_id':owner['authority']['application_id']}}}
        progress.update(stage='submit_original_stop',request=request)
        code,body=runtime_request('POST','/v1/runtime/operations',token,request,remaining())
        progress['admission']={'status':code,'body':body}
        require(code==202 and body.get('success') is True
                and (body.get('data') or {}).get('operation_id')==operation_id,
                'original interrupted Stop was not admitted',admission=progress['admission'])
        progress['stage']='observe_original_stop_ack'
        receipt_path=Path(owner['authority']['state_root'])/'operations'/(operation_id+'.json')
        while True:
            code,body=runtime_request('GET','/v1/runtime/operations/'+operation_id,
                                     token,timeout=min(1,remaining()))
            receipt=json.loads(receipt_path.read_text())
            progress['last_observation']={'status':code,'body':body,'receipt':receipt}
            barrier=stop_barrier_evidence(owner,business,request,code,body,receipt,ack,proc)
            if barrier is not None:
                break
            time.sleep(min(0.005,remaining()))
        # All checks and injection stay in this one container invocation.
        # Recheck the original operation and exact live service after owner
        # guards, then use the already-retained owner pidfd immediately.
        progress['stage']='recheck_before_exact_owner_signal'
        owner_proof=validate_captured_owner(owner,proc);fresh_identity()
        code,body=runtime_request('GET','/v1/runtime/operations/'+operation_id,
                                 token,timeout=min(1,remaining()))
        receipt=json.loads(receipt_path.read_text())
        barrier=stop_barrier_evidence(owner,business,request,code,body,receipt,ack,proc)
        require(barrier is not None,'physical Stop ACK disappeared before exact owner signal')
        owner_proof=validate_captured_owner(owner,proc)
        require(observe_business(business['pid'],owner,proc)==business
                and ack.read_bytes()==str(business['pid']).encode(),
                'captured business or ACK changed after final owner verification')
        remaining()
        progress.update(stage='send_exact_owner_signal',barrier=barrier,owner_proof=owner_proof)
        signal.pidfd_send_signal(fd,9)
        progress['signal']={'signal_sent':9,'pid':owner['pid'],'start_time':owner['start_time']}
        exited=bool(select.select([fd],[],[],remaining())[0])
        require(exited,'captured owner did not exit after exact SIGKILL',signal=progress['signal'])
        progress['stage']='captured_owner_exit_confirmed'
        return {'success':True,'operation_id':operation_id,'admission':progress['admission'],
                'business_capture':business,'supervisor':supervised,'barrier':barrier,
                'owner_proof':owner_proof,'signal':progress['signal'],
                'kill_proof':{'pidfd_readable':True,'pid':owner['pid'],
                              'start_time':owner['start_time']}}
    finally:
        os.close(fd)

if __name__=='__main__':
    progress={}
    try:
        result=controlled_stop(json.loads(sys.argv[1]),progress=progress)
    except Exception as error:
        print(json.dumps({'success':False,'error':str(error),'evidence':progress}),flush=True)
        raise SystemExit(1)
    print(json.dumps(result))
"""


R3_EVIDENCE_SCRIPT = OWNER_IDENTITY_CODE + """
import fcntl,hashlib,re,http.client,socket,xmlrpc.client

def retained_owner(owner,kernel,proc=Path('/proc')):
    root=Path(owner['authority']['state_root'])
    discovery=json.loads((root/'supervisor.json').read_text())
    require(discovery['instance']==owner['supervisor_id']
            and discovery['snapshot']['supervisor_id']==owner['supervisor_id']
            and discovery['snapshot']['binding']==owner['authority']['native']['binding'],
            'R3 original supervisor or workspace changed')
    require(kernel==owner['authority']['kernel']
            and json.loads((root/'identity.json').read_text())==kernel,
            'R3 original management instance changed')
    p=proc/str(owner['pid']);fields=(p/'stat').read_text().rsplit(')',1)[1].split()
    argv=[v.decode() for v in (p/'cmdline').read_bytes().split(bytes([0])) if v]
    exe=(p/'exe').stat()
    epoch='pid1:'+(proc/'sys/kernel/random/boot_id').read_text().strip()+':'+(proc/'1'/'stat').read_text().rsplit(')',1)[1].split()[19]
    require(fields[0]!='Z' and fields[19]==owner['start_time'] and argv==owner['argv']
            and [exe.st_dev,exe.st_ino]==owner['executable_id'] and epoch==owner['process_epoch'],
            'R3 original management process identity changed')
    lock=lock_evidence(root/'owner.lock',owner['pid'],proc)
    require(lock==owner['owner_lock'],'R3 stable owner lock changed')
    return {'pid':owner['pid'],'start_time':fields[19],'owner_lock':lock,
            'supervisor_id':discovery['instance'],'runtime_instance_id':kernel['runtime_instance_id']}

def journal_failure(payload,proc=Path('/proc')):
    root=Path(payload['authority']['state_root'])
    marker=root/'.deploy-recovery-required.json'
    if not marker.exists(): return {'verified':False,'waiting':'journal quarantine marker'}
    record=json.loads(marker.read_text())
    require(record['version']==1,'R3 invalid deployment recovery marker')
    for raw in record['originals']:
        backup=Path(raw)
        require(backup.parent==root and backup.name.startswith('.deploy-operation.corrupt-'),
                'R3 quarantine backup is outside the deployment authority')
        if not backup.exists() or backup.read_text()!=payload['damaged_bytes']: continue
        try:
            json.loads(backup.read_text())
        except json.JSONDecodeError as error:
            decoding_error=str(error)
        else: raise RuntimeError('R3 injected journal was not malformed JSON')
        log=Path(payload['log_path']).read_text()
        log=re.sub(r'\\x1b\\[[0-9;]*m','',log)
        lines=[line for line in log.splitlines()
               if 'rebuilding damaged deployment bookkeeping after supervisor takeover' in line
               and 'backup='+str(backup) in line and 'error=' in line]
        if not lines: return {'verified':False,'waiting':'original journal decoding diagnostic'}
        authority=payload['authority']
        if not authority['native'].get('generation'):
            return {'verified':False,'waiting':'original execution generation publication'}
        owner=observe_owner(authority,proc);owner['authority']=authority
        return {'verified':True,'owner':owner,'backup':str(backup),
                'damaged_sha256':hashlib.sha256(backup.read_bytes()).hexdigest(),
                'decoding_error':decoding_error,'owner_diagnostic':lines[-1]}
    return {'verified':False,'waiting':'this attempt original damaged journal backup'}

def stopped_engine(socket_path):
    class UnixConnection(http.client.HTTPConnection):
        def connect(self):
            self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            self.sock.settimeout(1);self.sock.connect(socket_path)
    class UnixTransport(xmlrpc.client.Transport):
        def make_connection(self,host): return UnixConnection(host)
    with xmlrpc.client.ServerProxy('http://localhost/RPC2',transport=UnixTransport()) as client:
        pid=client.supervisor.getPID();processes=client.supervisor.getAllProcessInfo()
    require(pid==1,'R3 engine is not the fixture container Supervisor')
    owned=[{'name':info['name'],'pid':info['pid'],'state':info['state']} for info in processes
           if info['name'].startswith('app-svc-') or info['name']=='app-pingap']
    require(all(info['pid']==0 and info['state'] in (0,100,200) for info in owned),
            'R3 managed business processes are not physically stopped',processes=owned)
    return {'supervisor_pid':pid,'processes':owned}

def native_stop_proof(payload,proc=Path('/proc')):
    owner=payload['owner'];root=Path(owner['authority']['state_root'])
    retained=retained_owner(owner,payload['kernel'],proc)
    request={'request_id':payload['request_id'],'action':'stop_work',
             'expected_generation':owner['generation']}
    discovery=json.loads((root/'supervisor.json').read_text())
    matches=[pair for pair in discovery['requests'] if pair[0]['request_id']==request['request_id']]
    require(len(matches)==1 and matches[0][0]==request,'R3 original native Stop request changed')
    receipt=matches[0][1];reply=payload['reply']
    for value in (receipt,reply):
        require(value['supervisor_id']==owner['supervisor_id']
                and value['binding']==owner['authority']['native']['binding']
                and value['operation_id']==request['request_id'] and value['intent']=='stopped',
                'R3 original native Stop receipt identity changed')
    progress={'complete':False,'request':request,'receipt':receipt,'reply':reply,'retained_owner':retained}
    if receipt['phase']!='stopped' or reply['phase']!='stopped': return progress
    work=root/'work'/owner['generation'];generation=json.loads((work/'generation.json').read_text())
    require(generation['id']==owner['generation'] and generation['supervisor']==owner['supervisor_id']
            and generation['worker_pid']==owner['pid']
            and generation.get('physical_domain')==owner['authority']['physical_domain']
            and generation.get('process_epoch')==owner['process_epoch'],
            'R3 original cleanup generation identity changed')
    require(generation['phase']=='Quiescent','R3 original generation has no aggregate Quiescent receipt')
    cleanup=json.loads((work/'cleanup-outcome.json').read_text())
    require(cleanup=={'outcome':'empty'},'R3 original cleanup is not Empty')
    gate=json.loads((work/'command-admission.json').read_text())
    require(gate['generation']==owner['generation'] and gate['accepting'] is False,
            'R3 original command authorization remains open')
    lock=work/'generation.lock';st=lock.stat()
    require([st.st_dev,st.st_ino]==owner['generation_lock']['file_id'],
            'R3 original stable generation lock changed')
    with lock.open('rb') as file:
        try: fcntl.flock(file,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError: return progress
        finally: fcntl.flock(file,fcntl.LOCK_UN)
    engine=json.loads((work/'supervisord-engine.json').read_text())
    require(engine['generation']==owner['generation'] and engine['supervisor_id']==owner['supervisor_id']
            and engine['socket']=='/var/run/supervisor.sock','R3 original engine receipt changed')
    engine_proof=stopped_engine(engine['socket'])
    # Read the same durable request again after the physical observations.
    after=json.loads((root/'supervisor.json').read_text())
    require(after['instance']==owner['supervisor_id']
            and [pair for pair in after['requests'] if pair[0]['request_id']==request['request_id']]==matches,
            'R3 original native Stop receipt changed during cleanup observation')
    retained_owner(owner,payload['kernel'],proc)
    progress.update(complete=True,quiescence={'generation':generation['id'],
                    'supervisor_id':generation['supervisor'],'phase':generation['phase'],
                    'physical_domain':generation['physical_domain'],'process_epoch':generation['process_epoch']},
                    cleanup=cleanup,engine=engine_proof)
    return progress

if __name__=='__main__':
    mode=sys.argv[1];payload=json.loads(sys.argv[2])
    if mode=='journal': result=journal_failure(payload)
    elif mode=='native': result=native_stop_proof(payload)
    elif mode=='retained': result=retained_owner(payload['owner'],payload['kernel'])
    else: raise RuntimeError('unsupported R3 evidence mode')
    print(json.dumps(result))
"""


def subprocess_failure_evidence(error):
    """Keep the failing subprocess diagnostics without exposing its argv/token."""
    def decoded(value):
        return value.decode(errors='replace') if isinstance(value, bytes) else value
    return {'type':type(error).__name__,'returncode':getattr(error,'returncode',None),
            'timeout':getattr(error,'timeout',None),
            'stdout':decoded(getattr(error,'stdout',None)),
            'stderr':decoded(getattr(error,'stderr',None))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='dev-rcoder-agent-runner:latest')
    parser.add_argument('--app-cli', required=True, type=Path)
    parser.add_argument('--file-server-proxy', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--source-dir', default='.', type=Path,
                        help='repo checkout used for SHA / dirty-status capture')
    args = parser.parse_args()
    for binary in (args.app_cli, args.file_server_proxy):
        if not binary.is_file():
            parser.error(f'missing binary: {binary}')
    try:
        docker_endpoint = require_local_docker_endpoint()
    except RuntimeError as error:
        parser.error(str(error))
    app = 'rcv' + uuid.uuid4().hex[:10]
    name = 'rcoder-app-cli-recovery-' + app
    volume = name + '-workspace'
    workspace = '/home/user/' + app
    state_root = '/home/user/.app-cli-state/' + app
    report = {'app_id': app, 'volume': volume, 'checks': [], 'containers': [],
              'docker_endpoint': docker_endpoint,
              'scenarios': {},
              'required_scenarios': ['A', 'B', 'C', 'G', 'R4', 'J', 'I',
                                     'R3', 'E', 'D', 'H', 'K'],
              'binaries': {
                  str(p.resolve()): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in (args.app_cli, args.file_server_proxy)}}
    # RV08：报告绑定源码身份（SHA + 脏改摘要）与镜像 digest。
    repo = Path(args.source_dir).resolve()
    git = lambda *argv: subprocess.run(['git', '-C', str(repo), *argv],
                                       capture_output=True, text=True, check=False)
    report['source'] = {
        'commit': git('rev-parse', 'HEAD').stdout.strip(),
        'dirty': git('status', '--short').stdout.strip().splitlines()[:20],
        'diff_stat': git('diff', '--stat', 'HEAD').stdout.strip()[:2000],
    }
    cid = None

    def docker(*argv, check=True, timeout=240):
        return subprocess.run(['docker', '--host', docker_endpoint, *argv], capture_output=True, text=True,
                              check=check, timeout=timeout)

    def execute(command, *argv, check=True):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv, check=check)

    def try_execute(command, *argv, timeout=30):
        return docker('exec', cid, 'sh', '-ec', command, '--', *argv,
                      check=False, timeout=timeout)

    def check(label, passed, detail=None, scenario=None):
        report['checks'].append({'name': label, 'ok': bool(passed), 'detail': detail})
        if scenario:
            report['scenarios'].setdefault(scenario, []).append(label)
        print(label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise RuntimeError(label)

    def write(files):
        code = ('import json,pathlib,sys; '
                '[(pathlib.Path(p).parent.mkdir(parents=True,exist_ok=True),'
                'pathlib.Path(p).write_text(t)) for p,t in json.loads(sys.argv[1]).items()]')
        docker('exec', cid, 'python3', '-c', code, json.dumps(files))

    def get(path, port=60000):
        body = json.loads(execute('curl -fsS --max-time 10 "$1"',
                                  f'http://127.0.0.1:{port}{path}').stdout)
        if not body.get('success'):
            raise RuntimeError(f'GET {path}: {body}')
        return body

    def post(action):
        data = json.dumps({'app_id': app})
        body = json.loads(execute(
            'curl -fsS --max-time 150 -H "content-type: application/json" '
            '--data "$1" "$2"', data,
            'http://127.0.0.1:60000/api/v1/userapp/dev/' + action).stdout)
        if not body.get('success'):
            raise RuntimeError(f'{action}: {body}')
        return body['data']

    def content():
        result = try_execute('curl -fsS --max-time 5 http://127.0.0.1:9080/')
        return result.stdout if result.returncode == 0 else None

    def start(action='start'):
        task = post(action)['task_id']
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            body = get('/api/v1/userapp/tasks/' + task + '?app_id=' + app)
            data = body.get('data') or {}
            status = data.get('status')
            if status == 'completed':
                return data
            if status in ('failed', 'cancelled'):
                raise RuntimeError(f'{action} task: {data}')
            time.sleep(0.4)
        raise RuntimeError(f'{action} task timeout')

    def identity():
        return get('/v1/runtime/identity', 3010)['data']['runtime_instance_id']

    def wait_management(deadline_seconds=90):
        # 统一 owner 的身份早于初始化应答（T1b）；这里的"管理可用"指
        # 启动恢复完成（deploy/status 200），与平台请求语义一致。
        deadline = time.monotonic() + deadline_seconds
        last = None
        while time.monotonic() < deadline:
            probe = try_execute(
                'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/deploy/status')
            if probe.returncode == 0:
                try:
                    return identity()
                except (subprocess.CalledProcessError, ValueError, KeyError):
                    pass
            last = probe.stdout[-100:] or last
            time.sleep(0.4)
        raise RuntimeError('management API did not initialize: ' + str(last))

    def owner_pid():
        result = try_execute('pgrep -f "[a]pp-cli serve" | head -1')
        return result.stdout.strip() or None

    def capture_live_owner():
        snapshot = json.loads(execute('app-cli owner status --workspace "$1"', workspace).stdout)
        authority = {'state_root': state_root, 'workspace': workspace,
                     'application_id': app, 'native': snapshot,
                     'kernel': get('/v1/runtime/identity', 3010)['data'],
                     'container_id': cid,
                     'physical_domain': {'authority': 'app-cli-recovery-v2',
                                         'instance_source_env': 'RCODER_PHYSICAL_POD_UID',
                                         'instance': report['containers'][-1]['domain_instance'],
                                         'volume': volume}}
        return json.loads(docker('exec', cid, 'python3', '-c', OWNER_PROCESS_SCRIPT,
                                 'capture', json.dumps(authority)).stdout)

    def signal_captured_owner(captured, sig):
        # This is controlled fault injection in our exact container. Validate
        # physical start time, exact file descriptors and actual kernel flock
        # holders before signalling through pidfd; a reused numeric PID is never
        # enough, and old guardian/worker ownership is explicitly rejected.
        if cid != captured['authority']['container_id']:
            raise RuntimeError('physical container changed before controlled signal')
        inspected = docker('inspect', '--format', '{{.Id}}', cid).stdout.strip()
        if inspected != cid:
            raise RuntimeError('captured physical container ID no longer exists')
        if identity() != captured['runtime_instance_id']:
            raise RuntimeError('management instance changed before controlled signal')
        return json.loads(docker('exec', cid, 'python3', '-c', OWNER_PROCESS_SCRIPT,
                                 'signal', json.dumps(captured), str(sig)).stdout)

    def interrupt_original_stop(captured, operation_id, revision):
        if cid != captured['authority']['container_id']:
            raise RuntimeError('physical container changed before controlled Stop')
        if docker('inspect', '--format', '{{.Id}}', cid).stdout.strip() != cid:
            raise RuntimeError('captured physical container no longer exists')
        payload = {'owner': captured, 'operation_id': operation_id,
                   'revision': revision, 'deploy_token': app + '-recovery-token'}
        try:
            result = docker('exec', cid, 'python3', '-c', STOP_CONTROLLER_SCRIPT,
                            json.dumps(payload), check=False, timeout=30)
        except subprocess.SubprocessError as error:
            report['r4_stop_controller'] = {'success': False, 'outcome_unknown': True,
                                          'error': type(error).__name__}
            report['subprocess_failure'] = subprocess_failure_evidence(error)
            raise RuntimeError('R4 Stop controller did not return; inspect the original receipt') from error
        try:
            evidence = json.loads(result.stdout)
        except ValueError:
            evidence = {'success': False, 'error': 'controller returned no valid evidence'}
        report['r4_stop_controller'] = evidence
        if result.returncode != 0 or evidence.get('success') is not True:
            report['subprocess_failure'] = {'type': 'R4StopControllerFailure',
                                           'returncode': result.returncode,
                                           'stdout': result.stdout, 'stderr': result.stderr}
            raise RuntimeError('R4 exact Stop controller: ' + evidence.get('error', 'failed'))
        return evidence

    def captured_process_gone(captured):
        code = """import json,sys
from pathlib import Path
try:
 fields=(Path('/proc')/sys.argv[1]/'stat').read_text().rsplit(')',1)[1].split()
 gone=fields[0]=='Z' or fields[19]!=sys.argv[2]
except FileNotFoundError:gone=True
print(json.dumps(gone))
"""
        return json.loads(docker('exec', cid, 'python3', '-c', code,
                                 str(captured['pid']), captured['start_time']).stdout)

    def supervisorctl(*argv, timeout=60):
        return docker('exec', cid, 'supervisorctl', *argv, check=False,
                      timeout=timeout)

    def new_container(instance=None):
        nonlocal cid
        image_id = docker('image', 'inspect', '--format', '{{.Id}}',
                          args.image).stdout.strip()
        domain = {'authority': 'app-cli-recovery-v2', 'volume': volume,
                  'instance_source_env': 'RCODER_PHYSICAL_POD_UID', 'instance': ''}
        instance = instance or str(uuid.uuid4())
        # RV08/E：fixture 程序经镜像自身的 supervisord 配置树装载（bind
        # mount 到 conf.d），容器 pid1 = supervisord（前台）——docker
        # restart 即真实入口自动重启，无需 pkill/手工拉服务。入口 wrapper
        # 先清理上一生命周期的 socket/pid 残留再 exec supervisord。
        conf_dir = Path(tempfile.mkdtemp(prefix='rcv-supervisor-'))
        (conf_dir / '40-recovery.conf').write_text(f'''[program:app-cli]
command=/usr/local/bin/app-cli serve --workspace {workspace}
directory={workspace}
autostart=true
exitcodes=0
autorestart=unexpected
startsecs=0
startretries=10
stopsignal=TERM
stopasgroup=true
killasgroup=true
stopwaitsecs=90
stdout_logfile=/home/user/logs/app-cli.out.log
redirect_stderr=true
[program:file-server-proxy]
command=/usr/local/bin/file-server-proxy --embed --policy all_rust --port 60000
autostart=true
autorestart=unexpected
startsecs=0
stdout_logfile=/tmp/proxy.log
redirect_stderr=true
''')
        cid = docker('create', '--name', name,
                     '--mount', f'type=volume,src={volume},dst=/home/user,volume-nocopy',
                     '--mount', f'type=bind,src={conf_dir}/40-recovery.conf,'
                     f'dst=/etc/supervisor/conf.d/40-recovery.conf',
                     '-e', f'PROJECT_ID={app}',
                     '-e', f'USERAPP_SINGLE_APP_ID={app}',
                     '-e', f'USERAPP_WORKSPACE_DIR={workspace}',
                     '-e', 'LOG_BASE_DIR=/home/user/logs',
                     '-e', f'APP_CLI_STATE_ROOT={state_root}',
                     '-e', f'RCODER_RUNTIME_IMAGE_DIGEST={image_id}',
                     '-e', 'RCODER_EXECUTION_DOMAIN=' + json.dumps(domain),
                     '-e', f'RCODER_PHYSICAL_POD_UID={instance}',
                     '-e', 'FILE_SERVER_LOG_DIR=/home/user/proxy-logs',
                     '-e', 'FILE_SERVER_APP_CLI_BIN=/usr/local/bin/app-cli',
                     # 与真实 builder 注入链同构（docker_manager B03）：
                     # 固定 serve owner 的复用路由需要部署凭据。
                     '-e', 'APP_CLI_MANAGED=1',
                     '-e', 'APP_CLI_DEPLOY_TOKEN=' + app + '-recovery-token',
                     '--entrypoint', 'sh',
                     image_id, '-ec',
                     'rm -f /var/run/supervisor.sock /var/run/supervisord.pid; '
                     f'mkdir -p /app/logs /home/user/logs {workspace}; '
                     'exec supervisord -n -c /etc/supervisor/supervisord.conf'
                     ).stdout.strip()
        report['containers'].append({'id': cid, 'domain_instance': instance})
        # docker create 允许先拷二进制再启动：程序首次拉起即有真实二进制。
        docker('cp', str(args.app_cli.resolve()), f'{cid}:/usr/local/bin/app-cli')
        docker('cp', str(args.file_server_proxy.resolve()),
               f'{cid}:/usr/local/bin/file-server-proxy')
        docker('start', cid)
        services_ready()
        return instance

    def services_ready(timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            probe = try_execute('test -S /var/run/supervisor.sock && '
                                'curl -fsS --max-time 2 http://127.0.0.1:60000/health')
            if probe.returncode == 0:
                return
            time.sleep(0.3)
        raise RuntimeError('file-server or supervisord did not initialize')

    def runtime_status():
        return get('/v1/runtime/status', 3010)['data']

    def runtime_post(operation_id, kind, revision, profile=None):
        """直连 owner 运行 API（3010）。返回 (status_code, body)。"""
        body = json.dumps({
            'operation_id': operation_id,
            'expected_runtime_instance_id': identity(),
            'expected_revision': revision,
            'workspace_id': app,
            'kind': kind,
            'profile': profile or {'profile': 'source',
                                   'input': {'workspace_id': app}},
        })
        result = try_execute(
            'curl -sS -o /tmp/rt-out.json -w "%{http_code}" --max-time 20 '
            '-X POST -H "content-type: application/json" '
            '-H "x-deploy-token: $1" --data "$2" "$3"',
            app + '-recovery-token', body,
            'http://127.0.0.1:3010/v1/runtime/operations', timeout=40)
        code = result.stdout.strip()
        payload = try_execute('cat /tmp/rt-out.json').stdout
        try:
            parsed = json.loads(payload)
        except ValueError:
            parsed = {'raw': payload}
        return int(code) if code.isdigit() else 0, parsed

    def runtime_get(operation_id):
        result = try_execute(
            'curl -sS -o /tmp/rt-out.json -w "%{http_code}" --max-time 10 '
            '-H "x-deploy-token: $1" "$2"',
            app + '-recovery-token',
            f'http://127.0.0.1:3010/v1/runtime/operations/{operation_id}',
            timeout=30)
        code = result.stdout.strip()
        payload = try_execute('cat /tmp/rt-out.json').stdout
        try:
            parsed = json.loads(payload)
        except ValueError:
            parsed = {'raw': payload}
        return int(code) if code.isdigit() else 0, parsed

    def wait_terminal(operation_id, timeout=150):
        deadline = time.monotonic() + timeout
        state = None
        while time.monotonic() < deadline:
            code, body = runtime_get(operation_id)
            state = ((body.get('data') or {}).get('state'))
            if state in ('succeeded', 'failed', 'cancelled', 'recovery_required'):
                return state
            time.sleep(0.5)
        raise RuntimeError(f'operation {operation_id} did not reach terminal: {state}')

    def app_files(marker):
        manifest = '''schema_version = 1
[project]
service_id = "web"
name = "Recovery matrix"
type = "python"
[build]
command = ["python3", "-m", "zipfile", "-c", "artifact.zip", "main.py"]
artifact = "artifact.zip"
[run]
command = ["python3", "main.py"]
migrate = ["python3", "-c", "open('migrations.log','a').write('ran\\\\n')"]
[health]
readiness_path = "/"
[proxy]
path = "/"
strip_prefix = false
'''
        devrun = ('\n[devrun]\ncommand = ["python3", "main.py"]\n')
        # G 屏障窗口：服务捕获 SIGTERM 后有界延迟退出——物理 Stop 执行期
        # 足够宽，受理屏障（pending Stop）期间的并发不同 Start 可靠落在
        # Busy 判定窗口内。
        main_py = ('import os,signal,time\n'
                   'from http.server import BaseHTTPRequestHandler,HTTPServer\n'
                   'def _bye(sig,frame):\n'
                   '    open('+repr(workspace + '/web/stop-ack')+',"w").write(str(os.getpid()))\n'
                   '    time.sleep(8)\n'
                   '    os._exit(0)\n'
                   'signal.signal(signal.SIGTERM,_bye)\n'
                   'class H(BaseHTTPRequestHandler):\n def do_GET(self):\n'
                   '  self.send_response(200)\n  self.end_headers()\n'
                   f'  self.wfile.write(b"{marker}")\n'
                   'HTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n')
        return {workspace + '/workspace.manifest.toml':
                    'schema_version=1\n[workspace]\nname="recovery"\n',
                workspace + '/web/project.manifest.toml': manifest + devrun,
                workspace + '/web/main.py': main_py,
                workspace + '/sentinel': marker}

    def migration_runs():
        result = try_execute(
            'wc -l < "$1/web/migrations.log" 2>/dev/null || echo 0', workspace)
        return int(result.stdout.strip() or 0)

    def r3_remaining(deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('R3 original receipt observation deadline exceeded')
        return remaining

    def r3_evidence(mode, payload, deadline):
        result = docker('exec', cid, 'python3', '-c', R3_EVIDENCE_SCRIPT,
                        mode, json.dumps(payload), timeout=r3_remaining(deadline))
        return json.loads(result.stdout)

    def r3_identity(deadline):
        remaining = r3_remaining(deadline)
        result = docker('exec', cid, 'curl', '-fsS', '--max-time', str(min(3, remaining)),
                        'http://127.0.0.1:3010/v1/runtime/identity', timeout=remaining)
        body = json.loads(result.stdout)
        if body.get('success') is not True:
            raise RuntimeError('R3 management identity is unavailable')
        return body['data']

    def degrade_with_bad_journal():
        """观察本次普通 deployment JSON 解码失败的隔离证据与同期管理身份。"""
        damaged_bytes = '{damaged-journal:r3-' + uuid.uuid4().hex
        write({state_root + '/.deploy-operation.json': damaged_bytes})
        supervisorctl('stop', 'app-cli')
        stopped = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('R3: supervisord program stopped cleanly before relaunch',
              stopped == '0', stopped, scenario='R3')
        supervisorctl('start', 'app-cli')
        deadline = time.monotonic() + 120
        evidence = {'verified': False}
        while time.monotonic() < deadline:
            probe = try_execute('curl -fsS --max-time 3 '
                                'http://127.0.0.1:3010/v1/runtime/identity',
                                timeout=r3_remaining(deadline))
            if probe.returncode == 0:
                kernel = json.loads(probe.stdout)['data']
                native = json.loads(docker('exec', cid, 'app-cli', 'owner', 'status',
                                          '--workspace', workspace,
                                          timeout=r3_remaining(deadline)).stdout)
                authority = {'state_root': state_root, 'workspace': workspace,
                             'application_id': app, 'native': native, 'kernel': kernel,
                             'container_id': cid,
                             'physical_domain': {'authority': 'app-cli-recovery-v2',
                                 'instance_source_env': 'RCODER_PHYSICAL_POD_UID',
                                 'instance': report['containers'][-1]['domain_instance'],
                                 'volume': volume}}
                evidence = r3_evidence('journal', {'authority': authority,
                    'damaged_bytes': damaged_bytes,
                    'log_path': '/home/user/logs/app-cli.out.log'}, deadline)
                if evidence['verified']:
                    report.setdefault('r3_journal_failures', []).append(evidence)
                    return evidence
            time.sleep(min(1, r3_remaining(deadline)))
        raise RuntimeError('R3 deployment journal decoding failure was not observed: '
                           + json.dumps(evidence))

    def complete_native_stop(owner, scenario="R3"):
        # CLI exit 0 observes acceptance. Keep one exact request and original
        # generation until its durable receipt and aggregate cleanup both settle.
        deadline = time.monotonic() + 60
        request_id = scenario.lower() + '-native-' + uuid.uuid4().hex
        attempt = {'owner': owner, 'request_id': request_id,
                   'generation': owner['generation']}
        report[scenario.lower() + '_native_attempt'] = attempt
        observed = json.loads(docker('exec', cid, 'python3', '-c', OWNER_PROCESS_SCRIPT,
                                     'capture', json.dumps(owner['authority']),
                                     timeout=r3_remaining(deadline)).stdout)
        if observed != owner:
            raise RuntimeError('R3 captured owner changed before original native Stop')
        while time.monotonic() < deadline:
            reply = json.loads(docker('exec', cid, 'app-cli', 'owner', 'stop',
                '--workspace', workspace, '--request-id', request_id,
                '--generation', owner['generation'], timeout=r3_remaining(deadline)).stdout)
            evidence = r3_evidence('native', {'owner': owner, 'request_id': request_id,
                'reply': reply, 'kernel': r3_identity(deadline)}, deadline)
            attempt['last_proof'] = evidence
            if evidence['complete']:
                remaining = r3_remaining(deadline)
                business = docker('exec', cid, 'curl', '-fsS', '--max-time',
                    str(min(5, remaining)), 'http://127.0.0.1:9080/',
                    check=False, timeout=remaining)
                if business.returncode == 0:
                    raise RuntimeError('R3 business HTTP still serves after original native Stop')
                evidence['business_http_exit_code'] = business.returncode
                return evidence
            # Poll the same receipt within the existing budget; no fresh Stop,
            # blind completion delay, admission bypass or general Busy retry.
            time.sleep(min(0.1, r3_remaining(deadline)))
        raise RuntimeError('R3 original native Stop did not settle: ' + json.dumps(attempt))

    try:
        docker('volume', 'create', volume)
        # ── A：并发调用方收敛到同一 owner ─────────────────────────────
        new_container()
        wait_management(120)
        write(app_files('recovery-a-1'))
        # RV08/A：先完成平台 start（release.lock 落盘），再以 agent 形态手动
        # run——第二个 CLI 入口必须显式转交给常驻 serve owner（dispatch 提交
        # 自身 Start），不出现第二编排/监听。锁竞争面由 J/C 场景覆盖。
        start('start')
        check('A: business HTTP serves content', content() == 'recovery-a-1',
              content(), scenario='A')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" > /tmp/manual-run.log 2>&1',
               '--', workspace)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            procs = execute(
                'ps -eo pid,args | grep "[a]pp-cli" | grep -v grep',
                check=False).stdout
            if 'app-cli run' in procs:
                break
            time.sleep(0.5)
        discovery = execute(
            'find /home/user/.app-cli-state -name supervisor.json').stdout.strip()
        check('A: exactly one supervisor discovery',
              len(discovery.splitlines()) == 1, discovery, scenario='A')
        first_identity = identity()
        listeners = execute(
            'pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('A: exactly one serve owner process', listeners == '1',
              procs, scenario='A')
        deadline = time.monotonic() + 90
        manual = ''
        while time.monotonic() < deadline:
            manual = try_execute('cat /tmp/manual-run.log').stdout
            if ('dispatching to the running owner' in manual
                    or 'dispatched start completed' in manual
                    or 'Error' in manual):
                break
            time.sleep(1)
        # 手动 run 的 Start 走"最后受理生效"接替：等待其自然终态后业务内容
        # 保持（同一 release 重编排）。
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if try_execute('pgrep -f "[a]pp-cli run" | wc -l'
                           ).stdout.strip() == '0':
                break
            time.sleep(1)
        manual = try_execute('cat /tmp/manual-run.log').stdout
        # RV08/A：裁决必须是**显式**转交/拒绝文案——"出现 owner 字样"
        # 这类宽松匹配不算数；第二监听冲突恒为失败。
        bound_conflict = ('address already in use' in manual.lower()
                          or ('bind' in manual.lower() and 'failed' in manual.lower()))
        handed_over = ('dispatching to the running owner' in manual
                       or 'dispatched start completed on the running owner' in manual)
        rejected = ('already has an owner' in manual
                    or 'operation is in progress' in manual.lower()
                    or 'conflict' in manual.lower()
                    or 'superseded' in manual.lower())
        check('A: concurrent run explicitly handed over or rejected',
              (handed_over or rejected) and not bound_conflict, manual[-500:],
              scenario='A')
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if content() == 'recovery-a-1':
                break
            time.sleep(1)
        check('A: business still serves after handover replacement',
              content() == 'recovery-a-1', content(), scenario='A')

        # ── B：owner TERM → 干净退出不复活；SIGKILL → supervisord 自愈 ──
        identity_before = first_identity
        stop_result = post('stop')
        check('B: stop confirms business stopped',
              stop_result.get('message') == 'Stopped' and content() is None,
              stop_result, scenario='B')
        check('B: management stays alive after business stop',
              identity() == identity_before, None, scenario='B')
        check('B: repeat stop idempotent',
              post('stop').get('message') == 'Stopped' and content() is None,
              None, scenario='B')
        pid = owner_pid()
        execute('kill -KILL "$1"', pid)
        # supervisord 拉起新 owner：完整初始化（含旧代次引擎清理围栏）后才
        # 能受理下一个业务请求。
        try:
            revived = wait_management(120)
        except RuntimeError:
            revived = None
        check('B: killed owner recovers via supervisord',
              revived is not None and revived != identity_before, revived,
              scenario='B')
        start('restart')
        check('B: business restarts after owner recovery',
              content() == 'recovery-a-1', content(), scenario='B')

        # ── C：同名辅助进程全部被杀（pkill app-cli）─────────────────
        write({workspace + '/web/main.py': app_files('recovery-c-2')[
            workspace + '/web/main.py']})
        execute('pkill -9 -f "[a]pp-cli" || true')
        try:
            recovered = wait_management(150)
        except RuntimeError:
            recovered = None
        check('C: management rebuilt after pkill of every app-cli',
              recovered is not None, recovered, scenario='C')
        start('restart')
        check('C: real rebuild changes HTTP content',
              content() == 'recovery-c-2', content(), scenario='C')
        check('C: workspace sentinel retained',
              execute('cat "$1/sentinel"', workspace).stdout == 'recovery-a-1',
              None, scenario='C')

        # ── G：Stop 执行屏障——不同 Start Busy 且零持久化；同 ID 重放 ──
        # RV08/G：真实受控 Stop（物理停进行中，pending 屏障持有期间）并发
        # 不同 Start：必须 Busy（附当前操作身份）且**零持久化**（不排队）。
        post('stop')
        start('restart')
        revision = runtime_status()['revision']
        stop_id = 'g-stop-' + uuid.uuid4().hex[:8]
        code, body = runtime_post(stop_id, 'stop', revision)
        check('G: runtime stop admitted', code == 202
              and (body.get('data') or {}).get('state') == 'accepted',
              body, scenario='G')
        # 屏障窗口内：不同 Start / 不同 Restart 均 Busy，零副作用。
        # （revision 用 stop 推进后的值——目标状态是停止完成后。）
        busy_start_id = 'g-start-' + uuid.uuid4().hex[:8]
        code_s, body_s = runtime_post(busy_start_id, 'start', revision + 1)
        active = body_s.get('active_operation_id')
        check('G: different start during stop execution is Busy',
              code_s == 409 and body_s.get('code') == 'ERR_OPERATION_IN_PROGRESS'
              and active == stop_id, body_s, scenario='G')
        busy_restart_id = 'g-restart-' + uuid.uuid4().hex[:8]
        code_r, body_r = runtime_post(busy_restart_id, 'restart', revision + 1)
        check('G: different restart during stop execution is Busy',
              code_r == 409 and body_r.get('code') == 'ERR_OPERATION_IN_PROGRESS',
              body_r, scenario='G')
        record = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, busy_start_id).stdout.strip()
        check('G: busy start left zero persisted record',
              record == '0', record, scenario='G')
        # 同 ID 重放：读回已受理进度（accepted/执行中），不重复受理。
        code_p, body_p = runtime_post(stop_id, 'stop', revision)
        check('G: same-id stop replays recorded state', code_p == 202
              and (body_p.get('data') or {}).get('operation_id') == stop_id,
              body_p, scenario='G')
        stop_state = wait_terminal(stop_id)
        check('G: admitted stop reaches terminal', stop_state == 'succeeded',
              stop_state, scenario='G')
        replay_code, replay = runtime_post(stop_id, 'stop', revision)
        check('G: same-id stop after terminal replays terminal state',
              replay_code == 202
              and (replay.get('data') or {}).get('state') == 'succeeded',
              replay, scenario='G')
        # Stop 完成后新 Start 正常执行。
        after_id = 'g-after-' + uuid.uuid4().hex[:8]
        code_a, body_a = runtime_post(after_id, 'start', revision + 1)
        check('G: fresh start after stop executes', code_a == 202, body_a,
              scenario='G')
        wait_terminal(after_id)
        check('G: fresh start serves content', content() == 'recovery-c-2',
              content(), scenario='G')

        # ── R4：run 冷启动复用常驻 serve，Stop 后管理保持同实例 ──────
        post('stop')
        supervisorctl('stop', 'app-cli')
        stopped = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('R4: supervised serve program stopped before run bootstrap',
              stopped == '0', stopped, scenario='R4')
        docker('exec', '-d', cid, 'sh', '-ec',
               'app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/r4run.log 2>&1; '
               'echo $? >/tmp/r4run.exit', '--', workspace)
        run_owner = wait_management(120)
        deadline = time.monotonic() + 150
        while time.monotonic() < deadline:
            if content() == 'recovery-c-2' and try_execute('cat /tmp/r4run.exit').stdout.strip() == '0':
                break
            time.sleep(0.2)
        check('R4: real run completes its fresh source request',
              content() == 'recovery-c-2' and try_execute('cat /tmp/r4run.exit').stdout.strip() == '0',
              try_execute('tail -c 2000 /tmp/r4run.log').stdout, scenario='R4')
        capture_r4 = capture_live_owner()
        ps_r4 = try_execute('ps -eo pid,args | grep "[a]pp-cli" | grep -v grep').stdout
        check('R4: run client leaves exactly one persistent serve owner',
              ps_r4.count('app-cli serve') == 1 and 'app-cli run --workspace' not in ps_r4,
              [ps_r4, capture_r4], scenario='R4')
        report['r4_diag'] = {'owner': capture_r4, 'api_identity': run_owner}
        start('restart')
        check('R4: platform source request reuses run-bootstrapped owner',
              content() == 'recovery-c-2' and identity() == run_owner,
              content(), scenario='R4')
        stop_r4 = post('stop')
        check('R4: Stop confirms business stopped and preserves the same management instance',
              stop_r4.get('message') == 'Stopped' and content() is None
              and identity() == run_owner and not captured_process_gone(capture_r4),
              stop_r4, scenario='R4')
        check('R4: repeated Stop stays idempotent without relaunch',
              post('stop').get('message') == 'Stopped' and content() is None
              and identity() == run_owner, None, scenario='R4')
        start('restart')
        check('R4: fresh restart executes on the retained owner without supervisor relaunch',
              content() == 'recovery-c-2' and identity() == run_owner,
              content(), scenario='R4')
        # Kill the captured management owner during a truly admitted physical
        # Stop. This checks an interrupted original operation, not a fictitious
        # lost HTTP acceptance reply (202 may already have reached the caller).
        retiring = capture_live_owner()
        signal_captured_owner(retiring, 15)
        deadline = time.monotonic() + 60
        while not captured_process_gone(retiring):
            if time.monotonic() >= deadline:
                raise RuntimeError('captured first run-bootstrapped owner did not shut down')
            time.sleep(0.1)
        docker('exec', '-d', cid, 'sh', '-ec',
               'app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/r4run2.log 2>&1; '
               'echo $? >/tmp/r4run2.exit', '--', workspace)
        run2_identity = wait_management(120)
        deadline = time.monotonic() + 150
        while time.monotonic() < deadline:
            if content() == 'recovery-c-2' and try_execute('cat /tmp/r4run2.exit').stdout.strip() == '0':
                break
            time.sleep(0.2)
        check('R4: second cold run starts actual source HTTP before Stop injection',
              content() == 'recovery-c-2' and run2_identity != run_owner
              and try_execute('cat /tmp/r4run2.exit').stdout.strip() == '0',
              None, scenario='R4')
        captured_lr = capture_live_owner()
        revision_lr = runtime_status()['revision']
        lost_id = 'r4-lost-' + uuid.uuid4().hex[:8]
        interrupted = interrupt_original_stop(captured_lr, lost_id, revision_lr)
        code_lr, body_lr = interrupted['admission']['status'], interrupted['admission']['body']
        check('R4: original interrupted Stop was really admitted',
              code_lr == 202 and (body_lr.get('data') or {}).get('operation_id') == lost_id,
              body_lr, scenario='R4')
        barrier = interrupted['barrier']
        check('R4: physical service Stop acknowledged before exact owner SIGKILL',
              barrier['business_alive']
              and barrier['ack']['bytes'] == str(interrupted['business_capture']['pid'])
              and barrier['operation']['operation_id'] == lost_id
              and barrier['operation']['runtime_instance_id'] == captured_lr['runtime_instance_id'],
              barrier, scenario='R4')
        deadline = time.monotonic() + 30
        while not captured_process_gone(captured_lr):
            if time.monotonic() >= deadline:
                raise RuntimeError('captured management owner survived SIGKILL')
            time.sleep(0.1)
        check('R4: captured management owner killed during original Stop',
              captured_process_gone(captured_lr) and interrupted['signal']['signal_sent'] == 9
              and interrupted['kill_proof']['pidfd_readable'],
              [captured_lr, interrupted['kill_proof']], scenario='R4')
        # 真实原收据：该 ID 恰好一条持久化记录（受理即落盘）。
        receipt_files = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, lost_id).stdout.strip()
        check('R4: exactly one persisted record for the lost-reply stop',
              receipt_files == '1', receipt_files, scenario='R4')
        # 新 serve 接管后按真实收据收束该 ID（未确认执行不伪造 Succeeded）。
        supervisorctl('start', 'app-cli')
        wait_management(150)
        settled = None
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            code_lr, body_lr = runtime_get(lost_id)
            settled = ((body_lr.get('data') or {}).get('state'))
            if settled in ('succeeded', 'failed', 'cancelled',
                           'recovery_required'):
                break
            time.sleep(0.5)
        check('R4: lost-reply stop settles from its real receipt',
              settled in ('succeeded', 'failed', 'recovery_required'), settled,
              scenario='R4')
        # 同 ID 重放（新实例身份）：不制造新操作（记录数不增）。
        replay_lr = runtime_post(lost_id, 'stop', revision_lr)
        records_lr = execute(
            'ls "$1/operations/" 2>/dev/null | grep -c "$2" || true',
            state_root, lost_id).stdout.strip()
        check('R4: same-id replay creates no new records',
              records_lr == '1', [replay_lr[0], records_lr], scenario='R4')
        start('restart')
        check('R4: business recovers after lost-reply reconciliation',
              content() == 'recovery-c-2', content(), scenario='R4')

        # ── J：挂死（SIGSTOP）owner 的有界边界 ──────────────────────
        pid = owner_pid()
        identity_at_freeze = identity()
        execute('kill -STOP "$1"', pid)
        stopped_probe = try_execute(
            'curl -fsS --max-time 6 http://127.0.0.1:3010/v1/runtime/identity',
            timeout=20)
        check('J: frozen owner does not answer management probes',
              stopped_probe.returncode != 0, stopped_probe.stdout[-200:],
              scenario='J')
        second = try_execute(
            'serve_rc=0; timeout 30 app-cli serve --workspace "$1" '
            '>/tmp/second-owner.log 2>&1 || serve_rc=$?; '
            'echo "second-owner-rc=$serve_rc"; tail -c 600 /tmp/second-owner.log',
            workspace, timeout=75)
        combined = (second.stdout or '') + (second.stderr or '')
        check('J: no second owner while lock is held by frozen process',
              'second-owner-rc=0' not in combined and 'owner' in combined.lower(),
              combined[-600:], scenario='J')
        execute('kill -CONT "$1"', pid)
        check('J: management responds again after SIGCONT',
              identity() == identity_at_freeze, None, scenario='J')
        check('J: business unaffected by freeze window',
              content() == 'recovery-c-2', content(), scenario='J')

        # ── I：app-11 升级 fixture（owner 停机导入，新 boot 读取）─────
        # RV08/I：先结束 owner（干净停程序，不复活），再导入脱敏 fixture，
        # 再启动新二进制——磁盘状态由下一次 owner 启动真实读取，不是活
        # owner 的内存覆盖。形态：RecoveryRequired discovery + 前容器域章
        # 的 Running 旧代次（无退出回执）并存。
        post('stop')
        supervisorctl('stop', 'app-cli')
        fixture_generation = '11111111-2222-4333-8444-555555555555'
        fixture_domain = {'authority': 'app-cli-recovery-v2', 'volume': volume,
                          'instance': 'old-pod-' + uuid.uuid4().hex[:8]}
        write({state_root + '/work/' + fixture_generation + '/generation.json':
                   json.dumps({'version': 1, 'id': fixture_generation,
                               'supervisor': 'dead-supervisor-from-fixture',
                               'token': 'fixture-token', 'intent': 'run',
                               'phase': 'Running', 'worker_pid': 424242,
                               'exit_code': None, 'error': None,
                               'physical_domain': fixture_domain}),
               state_root + '/supervisor.json':
                   json.dumps({'version': 2, 'instance': 'fixture-owner',
                               'address': '127.0.0.1:1', 'token': 'fixture',
                               'snapshot': {'version': 1,
                                            'binding': {'component': 'app-cli',
                                                        'resource': workspace},
                                            'supervisor_id': 'fixture-owner',
                                            'generation': fixture_generation,
                                            'phase': 'recovery_required',
                                            'intent': 'run',
                                            'operation_id': None,
                                            'error': 'generation cleanup is '
                                                     'unconfirmed: Running',
                                            'problem': None},
                                            'requests': []})})
        supervisorctl('start', 'app-cli')
        wait_management(150)
        start('restart')
        check('I: app-11 style fixture recovers without manual edits',
              content() == 'recovery-c-2', content(), scenario='I')
        preserved = json.loads(execute(
            'cat "$1"', state_root + '/work/' + fixture_generation +
            '/generation.json').stdout)
        check('I: previous-container Running record preserved as history',
              preserved['phase'] == 'Running'
              and preserved['physical_domain']['instance'].startswith('old-pod-'),
              preserved['phase'], scenario='I')

        # ── R3：坏 journal 期间的精确 Stop（HTTP + native）+ 管理可用 ──
        # 本次 deployment journal 的真实 JSON 解码失败由原隔离备份和
        # owner 日志共同证明；自动业务恢复受保护，管理身份同时可查询。
        # Stop 完成仍须依据其原操作和物理清理，不能用早期 identity 替代。
        degraded = degrade_with_bad_journal()
        check('R3: management survives confirmed deployment journal decoding failure',
              degraded['verified'], degraded, scenario='R3')
        stop_r3 = post('stop')
        check('R3: HTTP stop accepted during degraded business state',
              stop_r3.get('message') == 'Stopped', stop_r3, scenario='R3')
        id_probe2 = try_execute(
            'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
        check('R3: identity queryable after degraded stop',
              id_probe2.returncode == 0, id_probe2.stdout[-120:], scenario='R3')
        retained_r3 = r3_evidence('retained', {'owner': degraded['owner'],
            'kernel': get('/v1/runtime/identity', 3010)['data']}, time.monotonic() + 30)
        check('R3: HTTP Stop physically stops business on the same management owner',
              content() is None and retained_r3['owner_lock'] == degraded['owner']['owner_lock'],
              retained_r3, scenario='R3')
        # native StopWork（第二辆降级列车）：控制协议直连 owner。
        degraded = degrade_with_bad_journal()
        check('R3: native owner retains management during this journal decoding failure',
              degraded['verified'], degraded, scenario='R3')
        native_evidence = complete_native_stop(degraded['owner'])
        check('R3: native StopWork settles its original receipt and physical generation',
              native_evidence['complete'], native_evidence, scenario='R3')
        id_probe3 = try_execute(
            'curl -fsS --max-time 3 http://127.0.0.1:3010/v1/runtime/identity')
        check('R3: identity queryable after native stop',
              id_probe3.returncode == 0, id_probe3.stdout[-120:], scenario='R3')
        # RV03/F2 收口：损坏（无法解码）journal 由 owner 隔离为 .corrupt-*
        # 备份并重建——**不需要手工修复/删除**，下一显式新请求直接恢复。
        r3_restart = start('restart')
        report['r3_source_after_native_stop'] = r3_restart
        retained_r3 = r3_evidence('retained', {'owner': degraded['owner'],
            'kernel': get('/v1/runtime/identity', 3010)['data']}, time.monotonic() + 30)
        check('R3: business recovers from damaged journal without manual repair',
              content() == 'recovery-c-2'
              and retained_r3['owner_lock'] == degraded['owner']['owner_lock'],
              {'content': content(), 'retained_owner': retained_r3}, scenario='R3')
        corrupt_backup = execute(
            'ls "$1"/.deploy-operation.corrupt-*.json 2>/dev/null | wc -l',
            state_root).stdout.strip()
        check('R3: damaged journal preserved as corrupt backup',
              corrupt_backup != '0', corrupt_backup, scenario='R3')

        # ── E：同容器重启（docker restart，容器 ID 不变）────────────
        # RV08/E：pid1 是镜像自身 supervisord——restart 即真实入口自动重启
        # 服务（app-cli serve / file-server-proxy / PG / ttyd），无额外
        # pkill、无手工拉服务。
        post('stop')
        container_before_restart = docker('inspect', '--format', '{{.Id}}', cid).stdout.strip()
        docker('restart', cid)
        wait_management(150)
        container_after_restart = docker('inspect', '--format', '{{.Id}}', cid).stdout.strip()
        check('E: same-container restart preserves the inspected physical container ID',
              container_before_restart == cid == container_after_restart,
              [container_before_restart, container_after_restart], scenario='E')
        check('E: same-container restart keeps business stopped',
              content() is None, content(), scenario='E')
        start('restart')
        check('E: start after container restart works',
              content() == 'recovery-c-2', content(), scenario='E')

        # ── D：同卷容器重建（新容器、新物理实例身份）────────────────
        post('stop')
        docker('rm', '-f', cid)
        cid = None
        new_container()
        wait_management(120)
        start('restart')
        check('D: same-volume replacement recovers and serves',
              content() == 'recovery-c-2', content(), scenario='D')
        check('D: workspace data retained across replacement',
              execute('cat "$1/sentinel"', workspace).stdout == 'recovery-a-1',
              None, scenario='D')

        # ── H：应用迁移回执只诊断，实际失败日志与执行去重 ──────────
        # v2 R2/R5/R6：真实当前 identity 的 false/corrupt 回执允许新请求
        # 重新执行；只有真实脚本成功才确认。旧无关文件保持原字节。
        post('stop')
        owner_h = identity()
        receipt_dir = state_root + '/migration-receipts'

        def receipt_paths():
            code = 'import json,sys\nfrom pathlib import Path\nprint(json.dumps(sorted(str(p) for p in Path(sys.argv[1]).glob("*.json"))))\n'
            return json.loads(docker('exec', cid, 'python3', '-c', code, receipt_dir).stdout)

        def task_events(task):
            stream = execute('curl -fsS --max-time 15 "$1"',
                'http://127.0.0.1:60000/api/v1/userapp/tasks/' + task['id']
                + '/logs/stream?app_id=' + app + '&from_seq=0').stdout
            return [json.loads(line[6:]) for line in stream.splitlines()
                    if line.startswith('data: ')]

        def successful_operation(task):
            receipt = json.loads(execute('cat "$1/.deploy-operation.json"', state_root).stdout)
            operation_id = receipt['operation']['operation_id']
            code, body = runtime_get(operation_id)
            view = body.get('data') or {}
            check('H: task ' + task['id'] + ' and its original runtime operation succeed',
                  task['status'] == 'completed' and code == 200
                  and view.get('operation_id') == operation_id
                  and view.get('runtime_instance_id') == owner_h
                  and view.get('state') == 'succeeded',
                  {'task': task, 'operation': body}, scenario='H')
            return {'task': task, 'operation': body, 'events': task_events(task)}

        history_before = receipt_paths()
        check('H: real successful migration history exists before fault injection',
              bool(history_before), history_before, scenario='H')
        prior_runs = migration_runs()
        manifest_h = workspace + '/web/project.manifest.toml'
        previous_manifest_h = execute('cat "$1"', manifest_h).stdout
        assert 'name = "Recovery matrix"' in previous_manifest_h
        write({manifest_h: previous_manifest_h.replace(
            'name = "Recovery matrix"', 'name = "Advisory recovery matrix"')})
        baseline_h = start('restart')
        successful_operation(baseline_h)
        current_paths = sorted(set(receipt_paths()) - set(history_before))
        check('H: changed Source writes exactly one current release receipt',
              len(current_paths) == 1 and migration_runs() == prior_runs + 1,
              {'new_receipts': current_paths, 'runs': [prior_runs, migration_runs()]}, scenario='H')
        current_path = current_paths[0]
        current_receipt = json.loads(execute('cat "$1"', current_path).stdout)
        real_identity = current_receipt['identity']
        check('H: baseline current receipt is genuinely completed',
              current_receipt.get('completed') is True
              and current_path.endswith('/' + real_identity + '.json'),
              current_receipt, scenario='H')
        old_path = history_before[0]
        old_receipt = json.loads(execute('cat "$1"', old_path).stdout)
        unrelated_pending = json.dumps({'identity': old_receipt['identity'], 'completed': False})
        unrelated_corrupt_path = receipt_dir + '/legacy-corrupt.json'
        unrelated_corrupt = '{preserved-old-corrupt-receipt'
        write({old_path: unrelated_pending, unrelated_corrupt_path: unrelated_corrupt})
        report['h_advisory_attempts'] = []
        for fault, raw in [('pending', json.dumps({'identity': real_identity, 'completed': False})),
                           ('corrupt', '{current-corrupt-receipt')]:
            stop_h = post('stop')
            check('H: Stop before ' + fault + ' fault physically closes business on retained owner',
                  stop_h.get('message') == 'Stopped' and content() is None
                  and identity() == owner_h, stop_h, scenario='H')
            write({current_path: raw})
            before = migration_runs()
            task = start('restart')
            proof = successful_operation(task)
            report['h_advisory_attempts'].append({'fault': fault, **proof})
            receipt_now = json.loads(execute('cat "$1"', current_path).stdout)
            check('H: ' + fault + ' current receipt permits one actual migration and real HTTP',
                  content() == 'recovery-c-2' and identity() == owner_h
                  and migration_runs() == before + 1
                  and receipt_now == {'identity': real_identity, 'completed': True},
                  {'runs': [before, migration_runs()], 'receipt': receipt_now}, scenario='H')
            check('H: ' + fault + ' retry preserves unrelated pending and corrupt bytes',
                  execute('cat "$1"', old_path).stdout == unrelated_pending
                  and execute('cat "$1"', unrelated_corrupt_path).stdout == unrelated_corrupt,
                  {'pending_path': old_path, 'corrupt_path': unrelated_corrupt_path}, scenario='H')
            recovery_h = json.loads(execute(
                'curl -fsS --max-time 10 -H "x-deploy-token: $1" "$2"',
                app + '-recovery-token', 'http://127.0.0.1:3010/v1/runtime/recovery').stdout)['data']
            check('H: ' + fault + ' corrupt history stays observable without a recovery hold',
                  recovery_h['migrations'] == 'unreadable'
                  and not recovery_h['owner_protected'] and not recovery_h['kernel_protected'],
                  recovery_h, scenario='H')

        # 真实 exit 1，不伪造 Failed/Completed。完整 stdout/stderr 必须穿过
        # 原 operation -> task SSE；失败回执仍 false，业务真实 HTTP 仍可用。
        stop_h = post('stop')
        failure_manifest = execute('cat "$1"', manifest_h).stdout
        migration_line = next(line for line in failure_manifest.splitlines() if line.startswith('migrate = '))
        failure_program = 'import sys\nfrom pathlib import Path\nwith Path("migrations.log").open("a") as counter: counter.write("ran\\n")\nprint("H-ADVISORY-STDOUT-BEGIN",flush=True)\nif Path("migration-mode").read_text().strip()=="fail":\n    print("H-ADVISORY-STDERR-BEGIN"+"x"*24576+"H-ADVISORY-STDERR-END",file=sys.stderr,flush=True)\n    raise SystemExit(1)\nprint("H-ADVISORY-REPAIRED",flush=True)\n'
        before_failure_paths = set(receipt_paths())
        write({manifest_h: failure_manifest.replace(migration_line,
                  'migrate = ["python3", "migration-advisory.py"]'),
               workspace + '/web/migration-advisory.py': failure_program,
               workspace + '/web/migration-mode': 'fail'})
        before_failure_runs = migration_runs()
        failed_script_task = start('restart')
        failed_script_proof = successful_operation(failed_script_task)
        report['h_script_failure'] = failed_script_proof
        failure_paths = sorted(set(receipt_paths()) - before_failure_paths)
        check('H: exit 1 has a real unconfirmed current receipt and one script execution',
              len(failure_paths) == 1 and migration_runs() == before_failure_runs + 1,
              {'new_receipts': failure_paths, 'runs': [before_failure_runs, migration_runs()]}, scenario='H')
        failure_path = failure_paths[0]
        failed_receipt = json.loads(execute('cat "$1"', failure_path).stdout)
        check('H: exit 1 never fabricates migration completion while business really serves',
              failed_receipt.get('completed') is False
              and content() == 'recovery-c-2' and identity() == owner_h,
              failed_receipt, scenario='H')
        events = failed_script_proof['events']
        lines = '\n'.join(event.get('line', '') for event in events if event.get('event') == 'log')
        terminal = [event for event in events if event.get('event') in ('completed', 'failed', 'cancelled')]
        check('H: complete stdout and long stderr precede unique completed task SSE terminal',
              'H-ADVISORY-STDOUT-BEGIN' in lines
              and 'H-ADVISORY-STDERR-BEGIN' + 'x' * 24576 + 'H-ADVISORY-STDERR-END' in lines
              and 'ERROR run.migrate Exit' in lines
              and len(terminal) == 1 and terminal[0]['event'] == 'completed'
              and events[-1]['event'] == 'completed',
              {'task_id': failed_script_task['id'], 'events': events}, scenario='H')

        # 原生 Stop 仍验证原请求/代次、物理 Empty、真实 lock 释放；迁移诊断
        # 无需手工删除/翻 true，不改变 DBAdmin 未知写入或真实清理保护。
        native_h = complete_native_stop(capture_live_owner(), scenario='H')
        report['h_native_stop'] = native_h
        check('H: native Stop settles original receipt and physical generation with failed migration',
              native_h['complete'] and content() is None and identity() == owner_h,
              native_h, scenario='H')
        check('H: native Stop preserves the failed migration diagnostic',
              json.loads(execute('cat "$1"', failure_path).stdout) == failed_receipt,
              failed_receipt, scenario='H')
        repair_runs = migration_runs()
        write({workspace + '/web/migration-mode': 'success'})
        repaired_task = start('restart')
        successful_operation(repaired_task)
        check('H: new request repairs the script without manually confirming its receipt',
              content() == 'recovery-c-2' and migration_runs() == repair_runs + 1
              and json.loads(execute('cat "$1"', failure_path).stdout).get('completed') is True,
              {'runs': [repair_runs, migration_runs()]}, scenario='H')
        confirmed_runs = migration_runs()
        stop_after = post('stop')
        check('H: Stop after advisory recovery physically closes business',
              stop_after.get('message') == 'Stopped' and content() is None
              and identity() == owner_h, stop_after, scenario='H')
        final_task = start('restart')
        successful_operation(final_task)
        check('H: confirmed current migration remains deduplicated on fresh restart',
              content() == 'recovery-c-2' and identity() == owner_h
              and migration_runs() == confirmed_runs,
              {'runs': [confirmed_runs, migration_runs()]}, scenario='H')
        # K continues with the original successful migration manifest.
        write({manifest_h: previous_manifest_h})

        # ── K：.run 激活 + 制品 zip 缓存丢失的显式恢复 ────────────────
        # 真实入口全链：dev/restart 产物态构建（真 zip）→ owner Deploy
        # (ArtifactId) 激活 .run；切回源码态；删缓存后同制品重部署
        # （运行态 next_prepared / 空闲主循环两条消费路径）；journal 损坏
        # 隔离后前台 app-cli run <proj>/.run 恢复。
        def artifact_files(marker):
            files = app_files(marker)
            files[workspace + '/web/project.manifest.toml'] = \
                files[workspace + '/web/project.manifest.toml'].replace(
                    '\n[devrun]\ncommand = ["python3", "main.py"]\n', '')
            return files

        marker_k = 'recovery-k-1'
        post('stop')
        write(artifact_files(marker_k))
        start('restart')
        check('K: artifact-mode build deploys and serves',
              content() == marker_k, content(), scenario='K')
        built = execute('ls "$1"/builds/workspace-package-*.zip 2>/dev/null',
                        workspace).stdout.split()
        check('K: registered artifact zip exists after build',
              len(built) >= 1, built, scenario='K')
        release_k = built[-1].rsplit('workspace-package-', 1)[1][:-4]
        check('K: .run activated with the built release',
              release_k in execute('cat "$1/.run/release.lock.toml"',
                                   workspace).stdout, release_k, scenario='K')
        # 切回源码态（同 owner、业务运行中）。
        write(app_files('recovery-c-2'))
        start('restart')
        check('K: source mode switch back serves source content',
              content() == 'recovery-c-2', content(), scenario='K')
        # 清缓存 → 同制品重部署：源码态 owner 切换到已激活 .run（身份
        # 核验后的复用，不需要 zip）。
        execute('rm -f "$1"/builds/workspace-package-*.zip', workspace)
        revision_k = runtime_status()['revision']
        artifact_profile = {
            'profile': 'artifact',
            'input': {'artifact': {'source': 'artifact_id',
                                   'value': {'artifact_id': release_k}}}}
        k_switch = 'k-switch-' + uuid.uuid4().hex[:8]
        code_k, body_k = runtime_post(k_switch, 'deploy', revision_k,
                                      artifact_profile)
        check('K: cache-lost redeploy admitted while source serves',
              code_k == 202, body_k, scenario='K')
        check('K: cache-lost redeploy switches to the activated artifact',
              wait_terminal(k_switch) == 'succeeded'
              and content() == marker_k, content(), scenario='K')
        # 空闲路径：Stop 后同制品再部署（无 zip、业务已停）。
        post('stop')
        revision_k2 = runtime_status()['revision']
        k_idle = 'k-idle-' + uuid.uuid4().hex[:8]
        code_k2, body_k2 = runtime_post(k_idle, 'deploy', revision_k2,
                                        artifact_profile)
        check('K: idle cache-lost redeploy starts the business',
              code_k2 == 202 and wait_terminal(k_idle) == 'succeeded'
              and content() == marker_k, body_k2, scenario='K')
        # journal 损坏隔离 → run <proj>/.run 仍以平台授权源码为准；显式旧 Artifact redeploy 已在上面验证。
        post('stop')
        state_root_k = execute(
            'find /home/user -maxdepth 5 -name supervisor.json -printf "%h\n" '
            '| head -1').stdout.strip()
        write({state_root_k + '/.deploy-operation.json': '{damaged-k'})
        supervisorctl('stop', 'app-cli')
        stopped_k = try_execute('pgrep -f "[a]pp-cli serve" | wc -l').stdout.strip()
        check('K: serve stopped before current Source run through the .run alias',
              stopped_k == '0', stopped_k, scenario='K')
        docker('exec', '-d', cid, 'sh', '-ec',
               'exec app-cli run --workspace "$1" --log-dir /home/user/logs '
               '--admin-addr 0.0.0.0:3010 >/tmp/krun.log 2>&1',
               '--', workspace + '/.run')
        try:
            run_k = wait_management(150)
        except RuntimeError:
            run_k = None
        check('K: Source client bootstraps management despite damaged old journal',
              run_k is not None, run_k, scenario='K')
        k_content = None
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            k_content = content()
            if k_content == 'recovery-c-2':
                break
            time.sleep(1)
        check('K: Source run uses current source despite .run alias and missing old zip',
              k_content == 'recovery-c-2', k_content, scenario='K')
        # marker 释放在编排 readiness 确认（complete_running）之后——晚于
        # 业务首个 HTTP 应答，按预算轮询。
        marker_k_files = '1'
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            marker_k_files = execute(
                'ls "$1"/.deploy-recovery-required.json 2>/dev/null | wc -l',
                state_root_k).stdout.strip()
            if marker_k_files == '0':
                break
            time.sleep(2)
        check('K: replacement receipt released the recovery marker',
              marker_k_files == '0', marker_k_files, scenario='K')
        owner_k = identity()
        captured_k = capture_live_owner()
        stop_k = post('stop')
        check('K: Source Stop preserves the same manager and stops business',
              stop_k.get('message') == 'Stopped' and content() is None
              and identity() == owner_k and not captured_process_gone(captured_k),
              [stop_k, captured_k], scenario='K')
        start('restart')
        check('K: fresh restart after recovery executes Source on retained management',
              content() == 'recovery-c-2' and identity() == owner_k,
              content(), scenario='K')

        # RV08：必做场景清单完整性——任何未执行/无断言的场景都不算通过。
        executed = set(report['scenarios'])
        missing = [s for s in report['required_scenarios'] if s not in executed]
        check('matrix: every required scenario executed with assertions',
              not missing, missing, scenario='matrix')
        report['success'] = True
    except (Exception, KeyboardInterrupt) as error:
        report.update(success=False, error=str(error))
        if isinstance(error, subprocess.SubprocessError):
            report['subprocess_failure'] = subprocess_failure_evidence(error)
        if cid:
            ps_snapshot = try_execute(
                'ps -eo pid,ppid,args | head -40', timeout=30).stdout
            report['processes'] = ps_snapshot
            report['logs'] = try_execute(
                'python3 -c \'import sys\n'
                'from pathlib import Path\n'
                'files=[Path("/tmp/proxy.log"),Path("/tmp/manual-run.log"),'
                'Path("/tmp/second-owner.log"),Path("/tmp/r4run.log"),'
                'Path("/tmp/r4run2.log"),Path("/tmp/krun.log"),'
                'Path("/app/logs/supervisord.log")]'
                '+list(Path("/home/user/logs").rglob("*.log"))\n'
                'for p in files:\n'
                ' if p.is_file():\n'
                '  print(str(p)); print(p.read_text(errors="replace")[-6000:])\n\'',
                timeout=60).stdout
    finally:
        if cid:
            cleanup = docker('rm', '-f', cid, check=False)
            report['cleanup_ok'] = cleanup.returncode == 0
            report['success'] = report.get('success', False) and report['cleanup_ok']
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report.get('success') else 1


if __name__ == '__main__':
    raise SystemExit(main())
