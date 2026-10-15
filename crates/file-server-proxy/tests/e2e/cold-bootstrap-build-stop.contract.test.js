"use strict";
// Script contract tests only. These never create or mock successful containers.
const assert = require("node:assert/strict");
const test = require("node:test");
const { spawnSync } = require("node:child_process");
const {
  COLD_CONTROLLER, PROCESS_AND_LOCK, HELD_BUILD, PROCESS_GONE, checkColdEvidence,
} = require("./cold-bootstrap-build-stop.test.js");

// Execute the actual embedded program against a controlled /proc view. The
// owner.lock probe still uses a real fcntl exclusive lock. No process is
// signalled: only the injection syscall boundary is intercepted.
const LINUX_LOCK_FIXTURE = String.raw`import contextlib,fcntl,io,json,os,signal,sys,tempfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
program,case,mode=json.load(sys.stdin)
with tempfile.TemporaryDirectory(prefix='owner-lock-observation-') as directory:
 root=Path(directory);lock=root/'owner.lock';lock.write_text('stable owner lock')
 meta=lock.stat();pid=71;start='1024'
 kernel_key=('00:25' if case=='different_kernel_device' else format(os.major(meta.st_dev),'x')+':'+format(os.minor(meta.st_dev),'x'))+':'+str(meta.st_ino)
 if case=='different_kernel_device':assert (os.major(meta.st_dev),os.minor(meta.st_dev))!=(0,37),'fixture must model different stat and kernel devices'
 def row(kind='FLOCK',holder=pid,key=kernel_key,access='WRITE'):
  return '1: '+kind+' ADVISORY '+access+' '+str(holder)+' '+key+' 0 EOF'
 captured={'pid':pid,'start_time':start,'argv':['app-cli','serve','--control-only'],'generation':'owned-generation','supervisor_id':'owned-instance','physical_domain':'physical-domain','process_epoch':'process-epoch','source_root':'/owned-source','owner_lock':{'device':meta.st_dev,'inode':meta.st_ino}}
 record={'id':captured['generation'],'worker_pid':pid,'supervisor':captured['supervisor_id'],'phase':'Running','physical_domain':captured['physical_domain'],'process_epoch':captured['process_epoch']}
 discovery={'instance':captured['supervisor_id'],'snapshot':{'generation':captured['generation'],'binding':{'component':'app-cli','resource':captured['source_root']}}}
 (root/'supervisor.json').write_text(json.dumps(discovery));work=root/'work'/captured['generation'];work.mkdir(parents=True);(work/'generation.json').write_text(json.dumps(record))
 if case=='wrong_start_time':captured['start_time']='9999'
 reads={};calls=[]
 original_stat,original_text,original_bytes,original_iterdir=Path.stat,Path.read_text,Path.read_bytes,Path.iterdir
 def fake_stat(p,*args,**kwargs):
  if str(p)=='/proc/71/fd/8':
   return SimpleNamespace(st_dev=meta.st_dev,st_ino=meta.st_ino+(case=='foreign_fd'))
  return original_stat(p,*args,**kwargs)
 def fake_text(p,*args,**kwargs):
  name=str(p)
  if name=='/proc/locks':
   text=row(kind='POSIX' if case=='wrong_kernel_type' else 'FLOCK')
   if case=='multiple_holders':text+='\n'+row(holder=99)
   return text+'\n'
  if name=='/proc/71/stat':
   reads[name]=reads.get(name,0)+1
   current='9999' if case=='changed_start_time' and reads[name]>1 else start
   return str(pid)+' (app-cli) '+' '.join(['S']+['0']*18+[current])
  if name=='/proc/71/fdinfo/8':
   key='00:26:'+str(meta.st_ino) if case=='wrong_kernel_key' else kernel_key
   return 'pos: 0\nflags: 0100002\nmnt_id: 19\nino: '+str(meta.st_ino)+'\nlock:\t'+row(kind='POSIX' if case=='wrong_fd_type' else 'FLOCK',key=key,access='READ' if case=='read_lock' else 'WRITE')+'\n'
  return original_text(p,*args,**kwargs)
 def fake_bytes(p,*args,**kwargs):
  if str(p)=='/proc/71/cmdline':return b'app-cli\0serve\0--control-only\0'
  return original_bytes(p,*args,**kwargs)
 def fake_iterdir(p):
  if str(p)=='/proc':return iter([Path('/proc/71')])
  if str(p)=='/proc/71/fd':return iter([Path('/proc/71/fd/8')])
  return original_iterdir(p)
 def fake_pidfd(pid,flags=0):
  assert pid==71 and flags==0;calls.append(['pidfd_open',pid]);return os.open(os.devnull,os.O_RDONLY)
 def fake_signal(fd,signum,info=None,flags=0):calls.append(['pidfd_send_signal',signum])
 def fake_kill(pid,signum):calls.append(['bare_kill',pid,signum])
 output=io.StringIO();error=None
 with lock.open('r+') as held:
  fcntl.flock(held.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
  # A second open must fail independently; this is not a mocked busy flag.
  with lock.open('r+') as probe:
   try:fcntl.flock(probe.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
   except BlockingIOError:pass
   else:raise AssertionError('fixture failed to acquire an actual exclusive lock')
  with patch.object(Path,'stat',fake_stat),patch.object(Path,'read_text',fake_text),patch.object(Path,'read_bytes',fake_bytes),patch.object(Path,'iterdir',fake_iterdir),patch.object(os,'kill',fake_kill),patch.object(os,'pidfd_open',fake_pidfd,create=True),patch.object(signal,'pidfd_send_signal',fake_signal,create=True),patch.object(sys,'argv',['actual_program',str(root),json.dumps(captured)]),contextlib.redirect_stdout(output):
   try:exec(compile(program,'actual_linux_program','exec'),{'__name__':'__main__'})
   except (RuntimeError,AssertionError,FileNotFoundError,ValueError) as failure:error=str(failure)
 result=json.loads(output.getvalue()) if output.getvalue() else None
 print(json.dumps({'result':result,'error':error,'calls':calls,'stat_device':meta.st_dev,'kernel_key':kernel_key,'real_exclusive_busy':True}))
`;

function runLinuxLockFixture(program, name, mode = "observe") {
  const result = spawnSync("python3", ["-c", LINUX_LOCK_FIXTURE], {
    input: JSON.stringify([program, name, mode]), encoding: "utf8", timeout: 10000,
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout);
}

module.exports = { runLinuxLockFixture };

if (require.main === module) {

test("actual lock observation resolves kernel identity through the owner's exact descriptor", () => {
  const valid = runLinuxLockFixture(PROCESS_AND_LOCK, "different_kernel_device");
  assert.equal(valid.error, null);
  assert.equal(valid.real_exclusive_busy, true);
  assert.equal(valid.result.busy, true);
  assert.deepEqual(valid.result.holders, [71]);
  assert.deepEqual(valid.result.processes, [{ pid: 71, start_time: "1024" }]);
});

for (const name of ["foreign_fd", "changed_start_time", "wrong_fd_type", "wrong_kernel_type", "read_lock", "multiple_holders", "wrong_kernel_key"]) {
  test("actual lock observation rejects " + name, () => {
    const invalid = runLinuxLockFixture(PROCESS_AND_LOCK, name);
    assert(invalid.error || invalid.result?.holders?.length !== 1 || invalid.result.holders[0] !== 71, name + " was incorrectly accepted");
    assert.deepEqual(invalid.calls, [], name + " must never signal a process");
  });
}

function evidence() {
  const identity = { application_id: "private-app", workspace_id: "private-workspace", runtime_instance_id: "http-instance", protocol_version: 2, source_root: "/private-workspace", service_family: "userapp-dev", deployment_generation_id: "deployment-generation" };
  return [
    { busy: false, device: 19, inode: 810, holders: [], processes: [] },
    { busy: true, device: 19, inode: 810, holders: [71], processes: [{ pid: 71, start_time: "1024" }] },
    [{ pid: 71, start_time: "1024" }, { pid: 72, start_time: "1025" }],
    [{ pid: 71, exit_code: null }, { pid: 72, exit_code: 0 }],
    { instance: "native-instance", snapshot: { supervisor_id: "native-instance", binding: { component: "app-cli", resource: "/private-workspace" } } },
    { ...identity }, { ...identity }, identity,
  ];
}

test("cold owner evidence requires actual stable lock and matching captured caller", () => {
  assert.deepEqual(checkColdEvidence(...evidence()), { pid: 71, start_time: "1024" });
  for (const [label, mutate, message] of [
    ["old owner still holds lock", rows => { rows[0].busy = true; }, /genuinely released/],
    ["lock file was replaced", rows => { rows[1].inode++; }, /inode changed/],
    ["another device", rows => { rows[1].device++; }, /device changed/],
    ["two serve owners", rows => { rows[1].processes.push({ pid: 72, start_time: "1025" }); }, /more than one/],
    ["PID reused", rows => { rows[1].processes[0].start_time = "9999"; }, /captured caller/],
    ["unknown lock holder", rows => { rows[1].holders = [99]; }, /flock holder differs/],
  ]) {
    const rows = evidence(); mutate(rows);
    assert.throws(() => checkColdEvidence(...rows), message, label);
  }
});

test("losing cold caller must reuse successfully and both outcomes retain their identity", () => {
  for (const [label, mutate, message] of [
    ["lost result", rows => { rows[3].pop(); }, /both caller outcomes/],
    ["result substituted", rows => { rows[3][1].pid = 99; }, /captured identities/],
    ["both callers remain", rows => { rows[3][1].exit_code = null; }, /one caller/],
    ["reuse failed", rows => { rows[3][1].exit_code = 1; }, /successfully reuse/],
    ["native instance substituted", rows => { rows[4].instance = "old-native-instance"; }, /identity disagrees/],
    ["wrong component", rows => { rows[4].snapshot.binding.component = "other"; }, /another component/],
    ["wrong workspace", rows => { rows[4].snapshot.binding.resource = "/wrong"; }, /Expected values/],
    ["old endpoint", rows => { rows[5].runtime_instance_id = "old-http-instance"; }, /endpoint identity/],
    ["old identity file", rows => { rows[6].workspace_id = "other-workspace"; }, /saved identity/],
    ["identity omitted on both sides", rows => { for (const index of [5, 6, 7]) delete rows[index].application_id; }, /omitted application_id/],
    ["wrong deployment generation", rows => { rows[6].deployment_generation_id = "old-generation"; }, /saved identity/],
  ]) {
    const rows = evidence(); mutate(rows);
    assert.throws(() => checkColdEvidence(...rows), message, label);
  }
});

test("all actual Linux injection programs and nested caller compile without execution", () => {
  // compile() validates Linux-only /proc and fcntl snippets without executing
  // them on macOS or loading a Docker client. No generated bytecode is saved.
  const validate = String.raw`import ast,json,sys
for name,source in json.load(sys.stdin).items():
 tree=ast.parse(source,filename=name)
 compile(tree,name,'exec')
 if name=='cold_controller':
  nested=[n.value.value for n in tree.body if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='caller' for t in n.targets)]
  assert len(nested)==1
  compile(nested[0],'cold_caller','exec')
print('five Python programs compiled without execution')
`;
  const result = spawnSync("python3", ["-c", validate], {
    encoding: "utf8", timeout: 10000,
    input: JSON.stringify({ cold_controller: COLD_CONTROLLER, process_and_lock: PROCESS_AND_LOCK, held_build: HELD_BUILD, process_gone: PROCESS_GONE }),
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), "five Python programs compiled without execution");
});

}
