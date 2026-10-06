#!/usr/bin/env node
"use strict";
// Real private-container protocol contracts; no Cargo, AI or shared deployment.
const assert = require("node:assert/strict");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { UserappFixture } = require("./userapp-container-fixture.cjs");

const COLD_CONTROLLER = String.raw`import json,os,subprocess,sys,time
from pathlib import Path
root,workspace=Path(sys.argv[1]),sys.argv[2]
root.mkdir(parents=True,exist_ok=True)
caller=r'''import json,os,sys,time
from pathlib import Path
root,label,workspace=Path(sys.argv[1]),sys.argv[2],sys.argv[3]
stat=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()
(root/(label+'.ready')).write_text(json.dumps({'pid':os.getpid(),'start_time':stat[19]}))
deadline=time.monotonic()+120
while not (root/'go').is_file():
 if time.monotonic()>deadline:raise RuntimeError('caller release deadline expired')
 time.sleep(.01)
os.execv('/usr/local/bin/app-cli',['app-cli','serve','--control-only','--workspace',workspace,'--admin-addr','0.0.0.0:3010'])
'''
children=[]
for label in ('one','two'):
 output=(root/(label+'.log')).open('w')
 children.append(subprocess.Popen([sys.executable,'-c',caller,str(root),label,workspace],stdout=output,stderr=subprocess.STDOUT))
deadline=time.monotonic()+120
while not all((root/(label+'.ready')).is_file() for label in ('one','two')):
 if time.monotonic()>deadline:raise RuntimeError('caller ready deadline expired')
 time.sleep(.01)
(root/'ready.json').write_text(json.dumps([json.loads((root/(label+'.ready')).read_text()) for label in ('one','two')]))
while True:
 result=[{'pid':child.pid,'exit_code':child.poll()} for child in children]
 temporary=root/'result.tmp';temporary.write_text(json.dumps(result));temporary.replace(root/'result.json')
 if all(row['exit_code'] is not None for row in result):break
 time.sleep(.02)
`;

// stat(dev, ino) identifies the exact open file. The descriptor's fdinfo is
// authoritative for its kernel lock key; mount mappings may change st_dev.
const OWNER_LOCK_DESCRIPTOR = String.raw`def file_id(meta):return (meta.st_dev,meta.st_ino)
def kernel_key(value):
 parts=value.split(':')
 if len(parts)!=3:raise RuntimeError('invalid descriptor kernel lock key')
 return (int(parts[0],16),int(parts[1],16),int(parts[2]))
def require_process_instance(process,expected_argv):
 directory=Path('/proc')/str(process['pid'])
 fields=(directory/'stat').read_text().rsplit(')',1)[1].split()
 argv=(directory/'cmdline').read_bytes().split(b'\0')
 if len(fields)<20 or fields[0]=='Z' or fields[19]!=process['start_time'] or argv!=expected_argv:
  raise RuntimeError('captured process was reused or changed during lock observation')
def descriptor_lock(lock,meta,process,expected_argv):
 require_process_instance(process,expected_argv)
 directory=Path('/proc')/str(process['pid']);descriptors=[];keys=set()
 for fd in (directory/'fd').iterdir():
  try:opened=fd.stat()
  except FileNotFoundError:continue
  if file_id(opened)!=file_id(meta):continue
  info=(directory/'fdinfo'/fd.name).read_text()
  for line in info.splitlines():
   if not line.startswith('lock:'):continue
   fields=line.split()[1:]
   if len(fields)!=8 or fields[1:4]!=['FLOCK','ADVISORY','WRITE'] or fields[6:]!=['0','EOF'] or int(fields[4])!=process['pid']:
    raise RuntimeError('owner descriptor does not prove an exclusive FLOCK WRITE')
   key=kernel_key(fields[5])
   if key[2]!=meta.st_ino:raise RuntimeError('owner descriptor kernel inode differs from the stable lock')
   if file_id(fd.stat())!=file_id(meta):raise RuntimeError('owner descriptor changed during lock observation')
   keys.add(key);descriptors.append(int(fd.name))
 if len(keys)!=1:raise RuntimeError('owner lacks one verified stable lock descriptor kernel identity')
 key=next(iter(keys));holders=[]
 for line in Path('/proc/locks').read_text().splitlines():
  fields=line.split()
  if len(fields)<6 or fields[1]=='->':continue
  if kernel_key(fields[5])!=key:continue
  if len(fields)!=8 or fields[1:4]!=['FLOCK','ADVISORY','WRITE'] or fields[6:]!=['0','EOF']:
   raise RuntimeError('kernel lock is not the verified exclusive FLOCK WRITE')
  holders.append(int(fields[4]))
 if holders!=[process['pid']]:raise RuntimeError('actual flock holder differs from the verified owner descriptor')
 require_process_instance(process,expected_argv)
 if file_id(lock.stat())!=file_id(meta):raise RuntimeError('stable owner lock changed during observation')
 return {'pid':process['pid'],'start_time':process['start_time'],'fds':sorted(set(descriptors)),'kernel_key':list(key),'holders':holders}
`;

const PROCESS_AND_LOCK = String.raw`import fcntl,json,os,sys
from pathlib import Path
` + OWNER_LOCK_DESCRIPTOR + String.raw`
root=Path(sys.argv[1]);p=root/'owner.lock';before=p.stat()
with p.open('r+') as f:
 if file_id(os.fstat(f.fileno()))!=file_id(before):raise RuntimeError('stable lock changed before exclusive probe')
 try:fcntl.flock(f.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
 except BlockingIOError:busy=True
 else:busy=False;fcntl.flock(f.fileno(),fcntl.LOCK_UN)
processes=[];kernel_locks=[]
for d in Path('/proc').iterdir():
 if not d.name.isdigit():continue
 try:
  argv=(d/'cmdline').read_bytes().split(b'\0')
  stat=(d/'stat').read_text().rsplit(')',1)[1].split()
  if len(argv)>1 and Path(os.fsdecode(argv[0])).name=='app-cli' and argv[1]==b'serve' and stat[0]!='Z':
   process={'pid':int(d.name),'start_time':stat[19]};processes.append(process)
   if busy:kernel_locks.append(descriptor_lock(p,before,process,argv))
 except (FileNotFoundError,PermissionError,ProcessLookupError):pass
if file_id(p.stat())!=file_id(before):raise RuntimeError('stable owner lock changed during process scan')
holders=[pid for row in kernel_locks for pid in row['holders']]
print(json.dumps({'busy':busy,'device':before.st_dev,'inode':before.st_ino,'holders':holders,'processes':processes,'kernel_locks':kernel_locks}))
`;

const HELD_BUILD = String.raw`import json,os,signal,time,zipfile
from pathlib import Path
root=Path(__file__).resolve().parent;gate=root/'build-gate';gate.mkdir(exist_ok=True)
Path('builds.log').open('a').write('build\n')
if (gate/'armed').exists():
 (gate/'armed').rename(gate/'claimed')
 signal.signal(signal.SIGTERM,lambda *_:None)
 stat=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()
 (gate/'entered.json').write_text(json.dumps({'pid':os.getpid(),'start_time':stat[19]}))
 print('controlled-build-entered',flush=True)
 deadline=time.monotonic()+120
 while not (gate/'release').is_file():
  if time.monotonic()>deadline:raise RuntimeError('controlled build release deadline expired')
  time.sleep(.01)
 (gate/'completion.json').write_text(json.dumps({'pid':os.getpid(),'completed_after_release':True}))
with zipfile.ZipFile('artifact.zip','w') as archive:archive.write('main.py')
`;

const PROCESS_GONE = String.raw`import json,sys
from pathlib import Path
p=Path('/proc')/sys.argv[1]/'stat'
try:
 fields=p.read_text().rsplit(')',1)[1].split();gone=fields[0]=='Z' or fields[19]!=sys.argv[2]
except FileNotFoundError:gone=True
print(json.dumps(gone))
`;

function checkColdEvidence(before, after, callers, results, discovery, endpoint, saved, identity) {
  assert.equal(callers.length, 2, "exactly two independent callers must reach the barrier");
  assert.equal(new Set(callers.map(row => row.pid)).size, 2, "caller instances must be distinct");
  assert.equal(results.length, 2, "both caller outcomes must remain observable");
  assert.deepEqual(results.map(row => row.pid).sort(), callers.map(row => row.pid).sort(), "caller outcomes lost their captured identities");
  assert.equal(before.busy, false, "cold admission requires a genuinely released kernel lock");
  assert.equal(before.processes.length, 0, "no serve owner may remain before release");
  assert.equal(after.busy, true, "the winner must actually hold the exclusive kernel lock");
  assert.equal(after.device, before.device, "stable lock device changed");
  assert.equal(after.inode, before.inode, "stable lock inode changed");
  assert.equal(after.processes.length, 1, "two cold callers created more than one serve owner");
  const owner = after.processes[0];
  assert(callers.some(c => c.pid === owner.pid && c.start_time === owner.start_time), "owner is not a captured caller instance");
  assert.deepEqual(after.holders, [owner.pid], "actual flock holder differs from the serve process");
  assert.equal(results.filter(row => row.exit_code === null).length, 1, "one caller must remain the management owner");
  assert.equal(results.find(row => row.exit_code === null).pid, owner.pid);
  assert.equal(results.find(row => row.pid !== owner.pid).exit_code, 0, "the second serve must successfully reuse management");
  assert.equal(discovery.instance, discovery.snapshot.supervisor_id, "native discovery identity disagrees");
  assert.equal(discovery.snapshot.binding.component, "app-cli", "native discovery belongs to another component");
  assert.equal(discovery.snapshot.binding.resource, identity.source_root);
  for (const key of ["application_id", "workspace_id", "runtime_instance_id", "protocol_version"]) {
    assert(identity[key] !== undefined && identity[key] !== "", "management identity omitted " + key);
    assert.equal(endpoint[key], identity[key], "endpoint identity disagrees: " + key);
    assert.equal(saved[key], identity[key], "saved identity disagrees: " + key);
  }
  for (const key of ["source_root", "service_family", "deployment_generation_id"]) {
    assert(identity[key], "management identity omitted " + key);
    assert.equal(saved[key], identity[key], "saved identity disagrees: " + key);
  }
  return owner;
}

const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function waitFor(label, probe, budget = 90000) {
  const deadline = Date.now() + budget;
  let last;
  while (Date.now() < deadline) {
    try { const result = probe(); if (result) return result; } catch (error) {
      if (error.contractFailed) throw error;
      last = error;
    }
    await delay(100);
  }
  throw new Error(label + " timed out: " + (last?.message || "condition not observed"));
}

async function main() {
  // Pin this process to the same selected local engine before shared fixture
  // construction, without changing the user's Docker context.
  const repo = path.resolve(__dirname, "../../../..");
  const admitted = spawnSync("python3", ["-c", "import sys;sys.path.insert(0,sys.argv[1]);from isolated_docker import require_local_docker_endpoint;print(require_local_docker_endpoint())", path.join(repo, "tests-e2e/tools")], { encoding: "utf8" });
  assert.equal(admitted.status, 0, "local Docker admission failed");
  assert(admitted.stdout.trim().startsWith("unix:///"), "shared fixture requires a local Unix engine");
  process.env.DOCKER_HOST = admitted.stdout.trim();
  delete process.env.DOCKER_CONTEXT;
  const fixture = new UserappFixture("cold-build-stop", process.argv[2], process.argv[3], process.argv[4]);
  const observe = () => JSON.parse(fixture.exec(["python3", "-c", PROCESS_AND_LOCK, fixture.state]).stdout);
  const identity = () => fixture.request("/v1/runtime/identity", undefined, 3010);
  let failure;
  try {
    await fixture.create();
    fixture.prepare("obsolete-build-intent");
    await fixture.boot();
    const previousIdentity = identity();
    fixture.exec(["supervisorctl", "stop", "app-cli"]);
    const before = await waitFor("supervised serve stopped and kernel lock released", () => {
      const current = observe();
      return !current.busy && current.processes.length === 0 ? current : null;
    });
    fixture.check("cold bootstrap begins after the actual supervised owner exited", !before.busy && before.processes.length === 0, before);
    const cold = fixture.workspace + "/cold-callers";
    fixture.docker(["exec", "-d", fixture.cid, "python3", "-c", COLD_CONTROLLER, cold, fixture.workspace]);
    const callers = await waitFor("both cold callers reach the shared release barrier", () => JSON.parse(fixture.read(cold + "/ready.json")));
    assert.equal(callers.length, 2);
    assert.notEqual(callers[0].pid, callers[1].pid);
    fixture.write({ [cold + "/go"]: "release-two-independent-serve-callers" });
    const currentIdentity = await waitFor("cold management API initialized", () => {
      fixture.request("/v1/deploy/status", undefined, 3010);
      return identity();
    });
    const result = await waitFor("second cold serve finishes management reuse", () => {
      const rows = JSON.parse(fixture.read(cold + "/result.json"));
      if (rows.every(row => row.exit_code !== null)) throw Object.assign(new Error("both cold callers exited"), { contractFailed: true });
      return rows.filter(row => row.exit_code !== null).length === 1 ? rows : null;
    });
    const discovery = JSON.parse(fixture.read(fixture.state + "/supervisor.json"));
    const endpoint = JSON.parse(fixture.read(fixture.state + "/endpoint.json"));
    const saved = JSON.parse(fixture.read(fixture.state + "/identity.json"));
    const after = observe();
    const owner = checkColdEvidence(before, after, callers, result, discovery, endpoint, saved, currentIdentity);
    const native = JSON.parse(fixture.exec(["/usr/local/bin/app-cli", "owner", "status", "--workspace", fixture.workspace]).stdout);
    fixture.check("two synchronized cold serve callers converge to one real owner and stable flock", native.supervisor_id === discovery.instance && native.binding.component === "app-cli" && native.binding.resource === currentIdentity.source_root, { before, after, callers, result, native, api_identity: currentIdentity });
    fixture.check("cold owner is a new instance and management-only bootstrap leaves business stopped", currentIdentity.runtime_instance_id !== previousIdentity.runtime_instance_id && fixture.content().status === 7);

    const gate = fixture.workspace + "/web/build-gate";
    fixture.write({ [fixture.workspace + "/web/build.py"]: HELD_BUILD, [gate + "/armed"]: "hold-one-real-build" });
    const accepted = fixture.request("/api/v1/userapp/dev/start", { app_id: fixture.app });
    assert(accepted.task_id, "a real admitted build task is required");
    fixture.report.cancelled_build_admission = accepted;
    const entered = await waitFor("actual build child reaches the ready barrier", () => {
      const task = fixture.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${fixture.app}`);
      if (["completed", "failed", "cancelled"].includes(task.status)) {
        fixture.report.failed_task = task;
        throw Object.assign(new Error("build terminated before the ready barrier: " + task.status), { contractFailed: true });
      }
      return JSON.parse(fixture.read(gate + "/entered.json"));
    });
    const oldTask = fixture.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${fixture.app}`);
    const buildGoneBeforeStop = JSON.parse(fixture.exec(["python3", "-c", PROCESS_GONE, String(entered.pid), entered.start_time]).stdout);
    fixture.check("Stop injection targets a real running original build task", oldTask.id === accepted.task_id && oldTask.kind === "dev_start" && oldTask.status === "running" && oldTask.current_service === "web" && !buildGoneBeforeStop, { task: oldTask, build: entered });
    const stop = fixture.request("/api/v1/userapp/dev/stop", { app_id: fixture.app });
    // Release immediately after accepted Stop; this can produce a late success
    // callback while cancellation cleanup is still negotiating with the child.
    fixture.write({ [gate + "/release"]: "release-obsolete-build-after-stop" });
    fixture.check("Stop confirms no business while keeping the captured management instance", stop.message === "Stopped" && fixture.content().status === 7 && identity().runtime_instance_id === currentIdentity.runtime_instance_id);
    const cancelled = await waitFor("original task reaches its real cancelled terminal", () => {
      const task = fixture.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${fixture.app}`);
      if (task.status === "cancelled") return task;
      if (["completed", "failed"].includes(task.status)) throw Object.assign(new Error("old task terminal was " + task.status), { contractFailed: true });
      return null;
    });
    const wire = fixture.exec(["curl", "-fsS", "--max-time", "10", `http://127.0.0.1:60000/api/v1/userapp/tasks/${accepted.task_id}/logs/stream?app_id=${fixture.app}&from_seq=0`]).stdout;
    const events = wire.split("\n").filter(line => line.startsWith("data: ")).map(line => JSON.parse(line.slice(6)));
    fixture.check("original task GET/SSE keeps one cancelled terminal and no successful completion", cancelled.id === accepted.task_id && events.at(-1)?.event === "cancelled" && events.filter(event => ["cancelled", "failed", "completed"].includes(event.event)).length === 1, { task_id: accepted.task_id, events });
    await waitFor("captured old build process physically exits", () => JSON.parse(fixture.exec(["python3", "-c", PROCESS_GONE, String(entered.pid), entered.start_time]).stdout), 15000);
    const completion = JSON.parse(fixture.exec(["python3", "-c", "import json,sys;from pathlib import Path;p=Path(sys.argv[1]);print(p.read_text() if p.is_file() else 'null')", gate + "/completion.json"]).stdout);
    fixture.report.obsolete_build_completion = completion || { killed_before_completion: true };
    const until = Date.now() + 8000;
    let samples = 0;
    while (Date.now() < until) {
      assert.equal(fixture.content().status, 7, "late build callback resurrected business after Stop");
      assert.equal(identity().runtime_instance_id, currentIdentity.runtime_instance_id, "Stop replaced or lost management owner");
      assert.equal(fixture.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${fixture.app}`).status, "cancelled", "late callback overwrote original cancellation");
      samples++;
      await delay(100);
    }
    fixture.check("released obsolete build cannot resurrect HTTP or overwrite cancellation", samples > 1, { task_id: accepted.task_id, samples, observation_ms: 8000, completion: fixture.report.obsolete_build_completion });
    fixture.marker("fresh-start-after-cancel");
    await fixture.start("start", "fresh-start-after-cancel");
    assert.notEqual(fixture.report.checks.at(-1).evidence.task_id, accepted.task_id, "fresh Start must be a new task");
    await fixture.stop();
    const finalOwner = observe();
    fixture.check("fresh Start and Stop preserve the cold owner and stable lock inode", finalOwner.busy && finalOwner.device === after.device && finalOwner.inode === after.inode && finalOwner.processes.length === 1 && finalOwner.processes[0].pid === owner.pid && finalOwner.processes[0].start_time === owner.start_time && finalOwner.holders.length === 1 && finalOwner.holders[0] === owner.pid && identity().runtime_instance_id === currentIdentity.runtime_instance_id, finalOwner);
    fixture.check("the exact container and original volume data survive both contracts", JSON.parse(fixture.docker(["inspect", fixture.cid]).stdout)[0].Id === fixture.cid && fixture.read(fixture.workspace + "/sentinel") === fixture.id);
  } catch (error) { failure = error; }
  await fixture.finish(failure);
  console.log("COLD_BOOTSTRAP_BUILD_STOP_OK");
}

module.exports = { COLD_CONTROLLER, OWNER_LOCK_DESCRIPTOR, PROCESS_AND_LOCK, HELD_BUILD, PROCESS_GONE, checkColdEvidence };
if (require.main === module) main().catch(error => { console.error("COLD_BOOTSTRAP_BUILD_STOP_FAIL:", error.message); process.exitCode = 1; });
