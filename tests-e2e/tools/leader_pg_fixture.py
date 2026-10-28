#!/usr/bin/env python3
"""Prepare/run a run-owned local PostgreSQL leader fault test, without Cargo.

Only the captured test container is stopped/removed. Its named data volume is
retained. This tool never reads or restarts an existing RCoder/PG deployment.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid
from storage_contract_cases import passed_exactly_one

CASE = "background_tasks::pg_fault_tests::real_pg_session_loss_restarts_four_tasks_on_the_same_app_state"
DOCKER_CASE = "background_tasks::docker_metadata_tests::actual_docker_old_metadata_drop_preserves_same_name_new_cid_and_volume"


def command(*args, environment=None, check=True):
    result = subprocess.run(args, env=environment, text=True, capture_output=True, timeout=90)
    if check and result.returncode:
        raise RuntimeError(f"{args[0]} {args[1] if len(args)>1 else ''} failed: {result.stderr[:500]}")
    return result


def save(path, data):
    path.write_text(json.dumps(data, indent=2))
    path.chmod(0o600)


def owned(receipt):
    row = json.loads(command("docker", "inspect", receipt["container_id"]).stdout)[0]
    if row["Id"] != receipt["container_id"] or row["Config"]["Labels"].get("rcoder.e2e.run") != receipt["run_id"]:
        raise ValueError("captured PostgreSQL container ownership changed")
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "run", "cleanup"])
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--image", default="postgres:17")
    parser.add_argument("--test-binary", type=Path)
    parser.add_argument("--case", choices=["pg", "docker-metadata"], default="pg")
    args = parser.parse_args()
    root = args.directory.resolve()
    if args.action == "prepare":
        root.mkdir(parents=True, exist_ok=False)
        root.chmod(0o700)
        run = uuid.uuid4().hex
        image = json.loads(command("docker", "image", "inspect", args.image).stdout)[0]["Id"]
        volume = "rcoder-leader-data-" + run
        command("docker", "volume", "create", "--label", "rcoder.e2e.run=" + run, volume)
        environment = dict(os.environ, POSTGRES_PASSWORD=uuid.uuid4().hex)
        created = command("docker", "create", "--name", "rcoder-pg-leader-" + run[:16],
                          "--label", "rcoder.e2e.run=" + run,
                          "--publish", "127.0.0.1::5432", "--env", "POSTGRES_USER=contract",
                          "--env", "POSTGRES_DB=contract", "--env", "POSTGRES_PASSWORD",
                          "--volume", volume + ":/var/lib/postgresql/data", image, environment=environment)
        receipt = {"run_id": run, "container_id": created.stdout.strip(), "image_id": image,
                   "volume": volume, "data_retained": True}
        save(root / "ownership.json", receipt)
        owned(receipt)
        command("docker", "start", receipt["container_id"])
        deadline = time.monotonic() + 60
        while command("docker", "exec", receipt["container_id"], "pg_isready", "-U", "contract", "-d", "contract", check=False).returncode:
            if time.monotonic() >= deadline:
                raise TimeoutError("run-owned PostgreSQL did not become ready")
            time.sleep(0.5)
        address = command("docker", "port", receipt["container_id"], "5432/tcp").stdout.strip()
        if not address.startswith("127.0.0.1:") or "\n" in address:
            raise ValueError("PostgreSQL fixture must have exactly one loopback port")
        password = environment["POSTGRES_PASSWORD"]
        private = {"RCODER_PG_TEST_DSN": f"postgres://contract:{password}@{address}/contract",
                   "RCODER_PG_TEST_STRICT": "1", "RCODER_PG_FAULT_CONTAINER": receipt["container_id"],
                   "RCODER_PG_FAULT_RUN_ID": run, "RCODER_PG_FAULT_USER": "contract",
                   "RCODER_PG_FAULT_DATABASE": "contract", "RCODER_PG_FAULT_REPORT": str(root / "result.json")}
        private.update(RCODER_DOCKER_FAULT_IMAGE=image,
                       RCODER_DOCKER_FAULT_REPORT=str(root / "docker-metadata-result.json"))
        save(root / "environment.private.json", private)
        print(json.dumps({"prepared": True, "container_id": receipt["container_id"],
                          "image_id": image, "private_environment": str(root / "environment.private.json")}))
        return 0
    receipt = json.loads((root / "ownership.json").read_text())
    owned(receipt)
    if args.action == "cleanup":
        command("docker", "stop", "--time", "20", receipt["container_id"])
        command("docker", "rm", receipt["container_id"])
        receipt["container_removed"] = True
        save(root / "ownership.json", receipt)
        print(json.dumps({"cleaned": True, "retained_volume": receipt["volume"]}))
        return 0
    if args.test_binary is None:
        parser.error("run requires --test-binary (built separately by the caller)")
    environment = dict(os.environ, **json.loads((root / "environment.private.json").read_text()))
    environment.update(RCODER_DOCKER_FAULT_IMAGE=receipt["image_id"],
                       RCODER_DOCKER_FAULT_REPORT=str(root / "docker-metadata-result.json"))
    binary = args.test_binary.resolve()
    case = CASE if args.case == "pg" else DOCKER_CASE
    save(root / (args.case + "-binary.json"), {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "case": case})
    result = subprocess.run([str(binary), case, "--exact", "--ignored", "--nocapture"], env=environment,
                            text=True, capture_output=True, timeout=180)
    # The binary sees a private ephemeral credential; keep it out of diagnostics.
    password = environment["RCODER_PG_TEST_DSN"].split(":", 2)[2].split("@", 1)[0]
    output = (result.stdout + result.stderr).replace(password, "[REDACTED]")
    log_path=root / (args.case + "-test.log")
    log_path.write_text(output)
    log_path.chmod(0o600)
    report_path=root / ("result.json" if args.case=="pg" else "docker-metadata-result.json")
    report=json.loads(report_path.read_text()) if report_path.exists() else {}
    passed=passed_exactly_one(result.returncode,output) and report.get("success") is True
    print(json.dumps({"exit_code": result.returncode, "passed":passed,
                      "log": str(log_path), "data_retained": True}))
    return 0 if passed else (result.returncode or 1)


if __name__ == "__main__":
    raise SystemExit(main())
