#!/usr/bin/env python3
"""Real Unix binary contract; only subprocesses and paths created by this run."""
import argparse
import hashlib
import shlex
import threading
import urllib.request
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import uuid


def eventually(fn, seconds=20):
    until = time.monotonic() + seconds
    last = None
    while time.monotonic() < until:
        try:
            result = fn()
            if result:
                return result
        except (OSError, ValueError, AssertionError) as error:
            last = error
        time.sleep(0.1)
    raise AssertionError(f"condition not reached: {last}")


def child_pids(pid):
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    return [int(row.split()[0]) for row in rows.splitlines()
            if len(row.split()) == 2 and int(row.split()[1]) == pid]


def children(parent):
    # Parent is a live Popen owned by this run. PID is used only to inject a
    # fault into that parent's actual child, never to adopt or clear an owner.
    assert parent.poll() is None
    return child_pids(parent.pid)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    assert os.name == "posix", "this fault injection fixture requires Unix; not Windows evidence"
    binary = args.binary.resolve(strict=True)
    node = Path(shutil.which("node")).resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="rcoder-native-guardian-"))
    script = root / "ts.cjs"
    script.write_text("""
const fs=require('node:fs'), http=require('node:http'), cp=require('node:child_process');
const stop=process.env.NATIVE_FIXTURE_STOP, beat=process.env.NATIVE_FIXTURE_BEAT;
cp.spawn(process.execPath,['-e', `const fs=require('node:fs'); setInterval(()=>{if(fs.existsSync(process.env.NATIVE_FIXTURE_STOP))process.exit(0);fs.writeFileSync(process.env.NATIVE_FIXTURE_BEAT,String(Date.now()));},30);`],{stdio:'ignore'});
const server=http.createServer((req,res)=>res.end('ok')).listen(Number(process.env.PORT),'127.0.0.1');
setInterval(()=>{if(fs.existsSync(stop))process.exit(0)},30);
""")
    env = dict(os.environ, FILE_SERVER_PROXY_STATE_DIR=str(root / "state"),
               FILE_SERVER_PROXY_TS_NODE=str(node), FILE_SERVER_PROXY_TS_ENTRY=str(script),
               NATIVE_FIXTURE_STOP=str(root / "stop"), NATIVE_FIXTURE_BEAT=str(root / "beat"))
    env.pop("FILE_SERVER_PROXY_OWNER_SUPERVISOR", None)
    scope = root / "state" / "native-302e302e302e30-0"
    processes = []
    logs = []
    checks = []

    def control(action, identity, success=True):
        result = subprocess.run([str(binary), action, "--port", "0", "--instance-id", identity],
                                env=env, text=True, capture_output=True, timeout=70)
        assert (result.returncode == 0) == success, (action, result.returncode, result.stderr)
        return json.loads(result.stdout) if success else result.stderr

    def start(embed=False):
        launch = str(uuid.uuid4())
        log = open(root / f"{launch}.log", "w")
        logs.append(log)
        child_env = dict(env, FILE_SERVER_PROXY_LAUNCH_ID=launch)
        options = ["--policy", "all_ts"]
        if embed:
            child_env.pop("FILE_SERVER_PROXY_TS_NODE", None)
            child_env.pop("FILE_SERVER_PROXY_TS_ENTRY", None)
            child_env.update(USERAPP_WORKSPACE_DIR=str(root / "workspace"), FILE_SERVER_LOG_DIR=str(root / "logs"), LOG_BASE_DIR=str(root / "project-logs"))
            options = ["--policy", "all_rust", "--embed"]
        process = subprocess.Popen([str(binary), "start", "--port", "0", *options],
                                   env=child_env, stdout=log, stderr=log)
        processes.append((process, launch))
        def ready():
            assert process.poll() is None, f"supervisor exited; see {log.name}"
            result = subprocess.run([str(binary), "status", "--port", "0", "--instance-id", launch],
                                    env=env, text=True, capture_output=True, timeout=5)
            return result.returncode == 0 and json.loads(result.stdout)["phase"] == "Running"
        eventually(ready)
        return process, launch

    try:
        owner_parent, first = start()
        eventually(lambda: (root / "beat").exists())
        owner_pids = children(owner_parent)
        assert len(owner_pids) == 1, owner_pids
        os.kill(owner_pids[0], signal.SIGKILL)
        assert owner_parent.wait(timeout=20) != 0
        checks.append("real owner SIGKILL observed by retained parent Child")
        witness = next((scope / "supervisors").glob("*/witness.json"))
        assert json.loads(witness.read_text())["phase"] == "OwnerExited"
        guardians = list((scope / "work" / first / "guardians").glob("*/receipt.json"))
        assert guardians, "TS must be a guardian-owned process tree"
        eventually(lambda: all(json.loads(p.read_text())["phase"] == "Quiescent" for p in guardians))
        before = (root / "beat").read_text()
        time.sleep(0.2)
        assert (root / "beat").read_text() == before, "descendant still executes after guardian confirms"
        checks.append("pipe EOF drains real TS root and descendant before Quiescent")
        control("recover", str(uuid.uuid4()), False)
        assert control("recover", first)["phase"] == "Stopped"
        checks.append("same-boot recover requires original instance and real exit/cleanup witnesses")
        second_parent, second = start()
        control("stop", first, False)
        assert control("status", second)["phase"] == "Running"
        assert control("stop", second)["phase"] == "Stopped"
        assert second_parent.wait(timeout=20) == 0
        checks.append("late old-instance stop cannot stop successor; normal stop exits zero")
        third_parent, third = start(embed=True)
        address = control("status", third)["address"].replace("0.0.0.0", "127.0.0.1")
        (root / "workspace" / "fixture-app").mkdir(parents=True, exist_ok=True)
        command_script = root / "command.cjs"
        command_script.write_text("const fs=require('node:fs');setInterval(()=>{if(fs.existsSync(process.env.NATIVE_FIXTURE_STOP))process.exit(0);fs.writeFileSync(process.env.NATIVE_FIXTURE_BEAT,String(Date.now()));},30);")
        request = urllib.request.Request("http://" + address + "/api/v1/userapp/execute-command",
            data=json.dumps({"app_id":"fixture-app", "command":shlex.join([str(node), str(command_script)])}).encode(),
            headers={"Content-Type":"application/json"})
        request_result = []
        def execute():
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    request_result.append(response.read().decode())
            except Exception as error:
                request_result.append(type(error).__name__)
        request_thread = threading.Thread(target=execute, daemon=True)
        request_thread.start()
        command_root = scope / "work" / third
        eventually(lambda: list((command_root / "guardians").glob("*/receipt.json")))
        eventually(lambda: any(json.loads(p.read_text())["phase"] == "Running" for p in (command_root / "commands").glob("*.json")))
        assert list((command_root / "workers").glob("*.json")), "actual HTTP worker not tracked"
        mismatch_id = str(uuid.uuid4())
        mismatch_root = command_root / "guardians" / mismatch_id
        mismatch_root.mkdir()
        (mismatch_root / "receipt.json").write_text(json.dumps({"version":1,"id":mismatch_id,
            "instance_id":third,"phase":"Pending","command_record":None,"command_digest":"0"*64}))
        marker = root / "unauthorized-command"
        os_value = lambda text: {"Unix":list(os.fsencode(text))}
        wrong_spec = {"program":os_value(str(node)),
                      "args":[os_value("-e"),os_value("require('fs').writeFileSync(" + json.dumps(str(marker)) + ",'bad')")],
                      "env":[],"cwd":None}
        mismatch = subprocess.run([str(binary),"--native-command-guardian",str(mismatch_root)],
            input=json.dumps(wrong_spec) + "\n", text=True, capture_output=True, timeout=10)
        assert mismatch.returncode != 0 and "differs from original" in mismatch.stderr
        assert not marker.exists()
        assert json.loads((mismatch_root / "receipt.json").read_text())["phase"] == "Revoked"
        checks.append("live-owner authorization rejects changed command spec before side effect")
        os.kill(children(third_parent)[0], signal.SIGKILL)
        assert third_parent.wait(timeout=20) != 0
        request_thread.join(timeout=10)
        eventually(lambda: all(json.loads(p.read_text())["phase"] in ("Quiescent","Revoked") for p in (command_root / "guardians").glob("*/receipt.json")))
        denied_id = str(uuid.uuid4())
        denied_root = command_root / "guardians" / denied_id
        denied_root.mkdir()
        denied_receipt = denied_root / "receipt.json"
        denied_receipt.write_text(json.dumps({"version":1,"id":denied_id,"instance_id":third,
                                             "phase":"Pending","command_record":None,"command_digest":"0"*64}))
        denied = subprocess.run([str(binary), "--native-command-guardian", str(denied_root)],
                                input="{}\n", text=True, capture_output=True, timeout=10)
        assert denied.returncode != 0
        assert json.loads(denied_receipt.read_text())["phase"] == "Revoked"
        checks.append("unconsumed guardian refusal proves no spawn and persists Revoked")
        pending_id = str(uuid.uuid4())
        pending_root = command_root / "guardians" / pending_id
        pending_root.mkdir()
        pending_receipt = pending_root / "receipt.json"
        pending_receipt.write_text(json.dumps({"version":1, "id":pending_id, "instance_id":third,
                                               "phase":"Pending", "command_record":None,"command_digest":"0"*64}))
        assert control("recover", third)["phase"] == "Stopped"
        assert json.loads(pending_receipt.read_text())["phase"] == "Revoked"
        late = subprocess.run([str(binary), "--native-command-guardian", str(pending_root)],
                              input="{}\n", text=True, capture_output=True, timeout=10)
        assert late.returncode != 0
        assert json.loads(pending_receipt.read_text())["phase"] == "Revoked"
        checks.append("pending authorization revoked under lock rejects actual delayed guardian binary")
        assert not request_thread.is_alive() and request_result
        assert not any('"exit_code":0' in item for item in request_result)
        worker_values = [json.loads(p.read_text()) for p in (command_root / "workers").glob("*.json")]
        assert any(v.get("termination") == "OwnerExitedInterrupted" for v in worker_values)
        assert any(v.get("identity",{}).get("app_id") == "fixture-app" for v in worker_values)
        assert all(json.loads(p.read_text())["identity"]["app_id"] == "fixture-app" for p in (command_root / "commands").glob("*.json"))
        assert all(json.loads(p.read_text())["phase"] == "Quiescent" for p in (command_root / "commands").glob("*.json"))
        checks.append("actual embedded execute-command crash recovers command and interrupted workers without claiming HTTP success")

        retired_parent, retired = start(embed=True)
        retired_address = control("status", retired)["address"].replace("0.0.0.0", "127.0.0.1")
        request = urllib.request.Request("http://" + retired_address + "/api/v1/userapp/execute-command",
            data=json.dumps({"app_id":"fixture-app", "command":shlex.join([str(node), str(command_script)])}).encode(),
            headers={"Content-Type":"application/json"})
        request_result = []
        request_thread = threading.Thread(target=execute, daemon=True)
        request_thread.start()
        retired_work = scope / "work" / retired
        eventually(lambda: any(json.loads(p.read_text())["phase"] == "Running" for p in (retired_work / "commands").glob("*.json")))
        live_receipt = json.loads((scope / "owner.json").read_text())
        control_host, control_port = live_receipt["control_address"].rsplit(":",1)
        with socket.create_connection((control_host,int(control_port)), timeout=5) as forged:
            forged.sendall((json.dumps({"version":1,"instance_id":retired,"token":"wrong","action":"retire"}) + "\n").encode())
            assert forged.recv(1) == b"", "wrong token must not authorize retirement"
        control("retire", third, False)
        assert control("status", retired)["phase"] == "Running"
        assert control("retire", retired)["phase"] == "OwnerExited"
        assert retired_parent.wait(timeout=20) != 0
        retirement_receipt = json.loads((scope / "owner.json").read_text())
        assert retirement_receipt["retirement_requested"] is True
        assert retirement_receipt["phase"] == "Stopping", "retirement must not invent Stopped"
        assert control("retire", retired)["phase"] == "OwnerExited", "same retirement retry must observe original witness"
        eventually(lambda: all(json.loads(p.read_text())["phase"] == "Quiescent" for p in (retired_work / "guardians").glob("*/receipt.json")))
        assert control("recover", retired)["phase"] == "Stopped"
        request_thread.join(timeout=10)
        assert not request_thread.is_alive()
        checks.append("explicit original-owner retire closes actual long HTTP owner then recover uses its exit witness; retries do not target successor")

        fourth_parent, fourth = start()
        fourth_owner = children(fourth_parent)[0]
        guardian_pids = child_pids(fourth_owner)
        assert len(guardian_pids) == 1, guardian_pids
        os.kill(guardian_pids[0], signal.SIGKILL)
        os.kill(fourth_owner, signal.SIGKILL)
        assert fourth_parent.wait(timeout=20) != 0
        protected = list((scope / "work" / fourth / "guardians").glob("*/receipt.json"))
        original = [p.read_bytes() for p in protected]
        assert control("recover", fourth, False)
        assert [p.read_bytes() for p in protected] == original
        assert json.loads((scope / "owner.json").read_text())["phase"] != "Stopped"
        (root / "stop").touch()
        eventually(lambda: (root / "beat").exists())
        time.sleep(0.2)
        final_beat = (root / "beat").read_text()
        time.sleep(0.2)
        assert (root / "beat").read_text() == final_beat
        assert control("recover", fourth, False), "absence of heartbeat cannot replace guardian proof"
        checks.append("guardian-plus-owner crash remains protected even after fixture cooperatively stops")
        args.report.write_text(json.dumps({"status":"passed","binary":str(binary),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"fixture":str(root),"checks":checks},indent=2))
        print(json.dumps({"passed":len(checks),"report":str(args.report),"fixture":str(root)}))
    finally:
        # Cooperative stop file is recognized only by this run's Node fixture.
        (root / "stop").touch()
        for process, identity in processes:
            if process.poll() is None:
                try:
                    control("stop", identity)
                    process.wait(timeout=20)
                except Exception as error:
                    print(f"owned fixture cleanup remains unconfirmed at {root}: {error}")
        for log in logs:
            log.close()
        # Keep exact receipts/logs as evidence; never delete unknown state.


if __name__ == "__main__":
    main()
