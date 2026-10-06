"use strict";
// Contract and actual probe checks only; no Docker operations or UserApp E2E verdict.
const assert = require("node:assert/strict");
const test = require("node:test");
const { spawnSync } = require("node:child_process");
const {
  DURABLE_INPUT_PROBE, PUBLIC_INPUT_PROBE, LEGACY_REDACTION, AUTHENTICATED_MAIN,
  durableInputIsValid, publicInputIsPrivate,
} = require("./source-credential-recovery.test.js");

test("actual container probe programs compile without opening a Docker connection", () => {
  const source = "import ast,json,sys\nfor name,program in json.load(sys.stdin).items():compile(ast.parse(program,filename=name),name,'exec')\n";
  const result = spawnSync("python3", ["-c", source], {
    encoding: "utf8", timeout: 10000,
    input: JSON.stringify({ durable: DURABLE_INPUT_PROBE, public: PUBLIC_INPUT_PROBE, legacy: LEGACY_REDACTION, authenticated_business: AUTHENTICATED_MAIN }),
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
});

test("the real disk probe compares both inputs and emits only safe evidence", () => {
  const source = String.raw`import contextlib,io,json,os,sys,tempfile
from pathlib import Path
program=sys.stdin.read();secret='probe-private-comparison-only'
os.environ['POSTGRES_USER']='fixture_user';os.environ['POSTGRES_PASSWORD']=secret
with tempfile.TemporaryDirectory() as directory:
 root=Path(directory);(root/'operations').mkdir();path=root/'.deploy-operation.json';op=root/'operations'/'captured-op.json'
 expected={'username':'fixture_user','password':secret}
 # DeployRequest.runtime_operation_id is serde(skip); the durable identity is
 # Receipt.operation.operation_id, independent from the retained request.
 request={'execution_target':'source','run_pg':expected.copy()}
 deployment={'operation':{'operation_id':'captured-op'},'request':request.copy(),'active':{'artifact_release_id':'real-source-id','request':request.copy()}}
 operation={'request':{'operation_id':'captured-op','run_config':{'pg':expected.copy()}},'view':{'operation_id':'captured-op','state':'succeeded'}}
 def observe():
  output=io.StringIO()
  with contextlib.redirect_stdout(output):exec(compile(program,'actual-durable-probe','exec'),{'__name__':'__main__'})
  assert secret not in output.getvalue(),'probe itself exposed private input'
  return json.loads(output.getvalue())
 path.write_text(json.dumps(deployment));path.chmod(0o600);op.write_text(json.dumps(operation));op.chmod(0o600)
 sys.argv=['probe',str(root)];valid=observe()
 assert valid['deployment_input_matches'] and valid['operation_input_matches'] and valid['operation_identity_matches'] and valid['operation_succeeded']
 assert valid['deployment_mode']==valid['operation_mode']==0o600
 for record in (operation['request'],operation['view']):
  record['operation_id']='substituted-operation';op.write_text(json.dumps(operation));mismatched=observe()
  assert mismatched['operation_identity_matches'] is False,'substituted request/view identity was accepted'
  record['operation_id']='captured-op'
 operation['view']['state']='running';op.write_text(json.dumps(operation));unfinished=observe()
 assert unfinished['operation_succeeded'] is False,'nonterminal operation was accepted as successful'
 operation['view']['state']='succeeded';op.write_text(json.dumps(operation))
 deployment['request']['run_pg']={'username':'fixture_user','password':''};path.write_text(json.dumps(deployment));redacted=observe()
 assert redacted['deployment_input_matches'] is False
 path.chmod(0o644);permissive=observe();assert permissive['deployment_mode']==0o644
 print(json.dumps({'valid':valid,'redacted':redacted,'permissive':permissive}))
`;
  const result = spawnSync("python3", ["-c", source], { input: DURABLE_INPUT_PROBE, encoding: "utf8", timeout: 10000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  assert(!result.stdout.includes("probe-private-comparison-only"));
  const evidence = JSON.parse(result.stdout);
  assert.equal(evidence.valid.operation_id, "captured-op");
  assert.equal(evidence.valid.artifact_id, "real-source-id");
  assert.equal(evidence.redacted.deployment_input_matches, false);
  assert.equal(evidence.permissive.deployment_mode, 0o644);
});

test("durable acceptance rejects redaction, identity omission and permissive permissions", () => {
  const valid = { operation_id: "captured-op", artifact_id: "captured-artifact", execution_target: "source",
    deployment_input_matches: true, operation_input_matches: true, operation_identity_matches: true, operation_succeeded: true,
    deployment_mode: 0o600, operation_mode: 0o600, deployment_uid: 0, operation_uid: 0 };
  assert.equal(durableInputIsValid(valid), true);
  for (const change of [{ deployment_input_matches: false }, { operation_input_matches: false }, { operation_identity_matches: false }, { operation_succeeded: false },
    { operation_id: "" }, { artifact_id: "" }, { execution_target: "project_run" },
    { deployment_mode: 0o644 }, { operation_mode: 0o644 }, { deployment_uid: 1000 }, { operation_uid: 1000 }]) {
    assert.equal(durableInputIsValid({ ...valid, ...change }), false, JSON.stringify(change));
  }
});

test("public evidence requires every API and SSE route, authorization failure and no private fields", () => {
  const routes = ["/v1/runtime/status", "/v1/runtime/recovery", "/v1/runtime/recovery", "/v1/deploy/status",
    "/v1/runtime/operations/captured-op", "/v1/runtime/operations/captured-op/events",
    "/v1/runtime/operations/captured-op/events/stream", "/api/v1/userapp/tasks/task-id?app_id=fixture",
    "/api/v1/userapp/tasks/task-id/logs/stream?app_id=fixture&from_seq=0"];
  const valid = routes.map((route, index) => ({ route, authenticated: index !== 2, status: index === 2 ? 403 : 200,
    password_absent: true, internal_input_absent: true }));
  assert.equal(publicInputIsPrivate(valid), true);
  for (let index = 0; index < valid.length; index++) {
    assert.equal(publicInputIsPrivate(valid.filter((_, other) => other !== index)), false, "missing route " + routes[index]);
    for (const change of [{ password_absent: false }, { internal_input_absent: false }, { status: 500 }]) {
      assert.equal(publicInputIsPrivate(valid.map((row, other) => other === index ? { ...row, ...change } : row)), false);
    }
  }
});
