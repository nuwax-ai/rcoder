#!/usr/bin/env python3
"""Compile a frozen checkout, then run strict cases on an owned Docker plane.

Runtime bases are explicit immutable inputs. They must already provide the
repository's compiler/runtime prerequisites; absent prerequisites are failures.
This entry never uses an existing RCODER_URL or updates a shared deployment.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import uuid

from compute_failure_contract import validate_build
from prod_readiness_contract import source_identity
from turso_runtime_contract import command, isolated_config, service_config, validate_owned, wait_ready

ROOT = Path(__file__).resolve().parents[2]
CASES = (
    ("compose_lifecycle", "computer_owner_conflict_contract"),
    ("compose_userapp_dev", "userapp_dev_files_two_entry_points"),
    ("compose_userapp_dev", "userapp_dev_git_full_outbound_shape"),
    ("compose_userapp_dev", "userapp_dev_server_lifecycle"),
    ("compose_userapp_dev", "userapp_manual_owner_multi_process_control"),
    ("custom_page_preview", "cpp_lifecycle_envelopes"),
    ("custom_page_preview", "cpp_preview_http"),
    ("custom_page_preview", "cpp_hmr_websocket"),
    ("compose_userapp_dev", "userapp_dev_idle_recycle_owner_recovery"),
    ("compose_userapp_faults", "userapp_compute_stop_timeout_recovery_contract"),
    ("compose_userapp_faults", "userapp_compute_late_stop_cid_absence_recovery_contract"),
)


def immutable_image(value):
    if not re.fullmatch(r"(?:[A-Za-z0-9._:/-]+@)?sha256:[0-9a-f]{64}", value):
        raise ValueError("CI bases must be immutable image IDs or registry digest references")
    return value


def client_target_path(report_root, run_id):
    target = Path(os.environ.get("CARGO_TARGET_DIR") or
                  report_root.resolve().parent / ("rcoder-ci-client-target-" + run_id)).expanduser().resolve()
    if target.is_relative_to(report_root.resolve()):
        raise ValueError("host Cargo cache must be outside the uploaded evidence directory")
    return target


def fingerprint(path):
    if path.is_symlink():
        return {"link": os.readlink(path)}
    if path.is_file():
        return {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    return {"missing": True}


def containers():
    ids = command("docker", "ps", "-aq", "--no-trunc").split()
    rows = json.loads(command("docker", "inspect", *ids)) if ids else []
    return {row["Id"]: {"id": row["Id"], "name": row["Name"], "image": row["Image"],
                       "running": row["State"]["Running"]} for row in rows}


def private_children(folder, controller):
    ids = command("docker", "ps", "-aq", "--no-trunc").split()
    rows = json.loads(command("docker", "inspect", *ids)) if ids else []
    return [row["Id"] for row in rows if row["Id"] != controller and any(
        mount.get("Type") == "bind" and Path(mount.get("Source", "/")).is_relative_to(folder)
        for mount in row["Mounts"])]


def successful_case(exit_code, summary, case):
    rows = summary.get("results") or []
    return (exit_code == 0 and len(rows) == 1
            and rows[0].get("test") == case and rows[0].get("verdict") == "pass"
            and not rows[0].get("errors"))


def finish_owned_run(record, folder, compose, controller, before, source, execute, save):
    """Preserve the primary failure even when diagnostics or cleanup fail."""
    errors = []
    record["cleanup_confirmed"] = False
    if controller:
        owned = False
        try:
            validate_owned(json.loads(command("docker", "inspect", controller))[0], record)
            owned = True
        except Exception as error:
            errors.append("confirm controller identity: " + str(error))
        if owned:
            try:
                execute(["docker", "logs", "--tail", "500", controller], "controller-stdout")
            except Exception as error:
                errors.append("capture controller diagnostics: " + str(error))
            children = None
            try:
                children = private_children(folder, controller)
                record["leftover_builder_ids"] = children
            except Exception as error:
                errors.append("observe remaining owned builders: " + str(error))
            if children == []:
                stopped = False
                try:
                    execute(compose + ["stop", "rcoder"], "compose-stop")
                    stopped_row = json.loads(command("docker", "inspect", controller))[0]
                    validate_owned(stopped_row, record)
                    state = stopped_row.get("State", {})
                    if (state.get("Running") is not False or state.get("Restarting") is not False
                            or state.get("Pid") != 0):
                        raise RuntimeError("owned controller execution is not confirmed stopped")
                    record["stopped_controller_state"] = state
                    stopped = True
                except Exception as error:
                    errors.append("confirm stopped controller: " + str(error))
                if stopped:
                    try:
                        # An accepted Create can complete while the controller
                        # stops. Dynamic builders do not carry Compose labels.
                        children_after_stop = private_children(folder, controller)
                        record["leftover_builder_ids_after_stop"] = children_after_stop
                        if children_after_stop:
                            errors.append("owned builders appeared while stopping the controller")
                        else:
                            execute(compose + ["down", "--remove-orphans"], "compose-down")
                            children_after_down = private_children(folder, controller)
                            record["leftover_builder_ids_after_down"] = children_after_down
                            compose_ids = command("docker", "ps", "-aq", "--filter", "label=com.docker.compose.project=" + record["project"])
                            record["cleanup_confirmed"] = not compose_ids and not children_after_down
                            record["retained_volume"] = record["project"] + "_data"
                            if children_after_down:
                                errors.append("owned builders appeared during Compose cleanup")
                    except Exception as error:
                        errors.append("remove owned stopped controller: " + str(error))
    try:
        after = containers()
        record["preexisting_containers_preserved"] = bool(record.get("baseline_confirmed")) and all(after.get(key) == value for key, value in before.items())
    except Exception as error:
        record["preexisting_containers_preserved"] = False
        errors.append("verify existing containers: " + str(error))
    try:
        record["source_after"] = source_identity(ROOT)
    except Exception as error:
        errors.append("verify source: " + str(error))
    record["cleanup_errors"] = errors
    record["success"] = bool(record.get("cases_passed") and record.get("cleanup_confirmed")
        and record.get("preexisting_containers_preserved") and record.get("source_after") == source
        and not record.get("error") and not errors)
    save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller-base", required=True, type=immutable_image)
    parser.add_argument("--builder-base", required=True, type=immutable_image)
    parser.add_argument("--report-root", required=True, type=Path)
    args = parser.parse_args()
    source = source_identity(ROOT)
    run_id, case_id = uuid.uuid4().hex, uuid.uuid4().hex
    folder = args.report_root.resolve() / run_id
    folder.mkdir(parents=True, mode=0o700)
    snapshot = folder / "source"
    snapshot.mkdir()
    before = {}
    record = {"run_id": run_id, "case_id": case_id, "source": source,
              "containers_before": before, "success": False, "scenarios": [],
              "volumes_removed": False, "started_at": time.time()}
    project = "rcoder-ci-" + run_id
    record["project"] = project
    compose = ["docker", "compose", "-p", project, "-f", str(folder / "compose.json")]
    controller = None

    def save():
        (folder / "result.json").write_text(json.dumps(record, indent=2) + "\n")

    def execute(argv, name):
        with (folder / (name + ".log")).open("w") as log:
            result = subprocess.run(argv, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(name + " failed; see owned report log")

    try:
        client_target = client_target_path(args.report_root, run_id)
        record["host_client_target_dir"] = str(client_target)
        before = containers()
        record["containers_before"] = before
        record["baseline_confirmed"] = True
        save()
        names = sorted(set(os.fsdecode(name) for name in subprocess.check_output([
            "git", "-C", str(ROOT), "ls-files", "-z", "--cached", "--others", "--exclude-standard"
        ]).split(b"\0") if name))
        manifest = {name: fingerprint(ROOT / name) for name in names}
        for name in names:
            original, frozen = ROOT / name, snapshot / name
            if original.is_symlink():
                frozen.parent.mkdir(parents=True, exist_ok=True)
                frozen.symlink_to(os.readlink(original))
            elif original.is_file():
                frozen.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(original, frozen)
            if fingerprint(frozen) != manifest[name]:
                raise ValueError("source snapshot mismatch: " + name)
        if source_identity(ROOT) != source:
            raise ValueError("source changed while freezing")
        manifest_path = folder / "input-manifest.json"
        manifest_path.write_text(json.dumps(manifest, indent=2))
        bases = {}
        for role, ref in (("controller", args.controller_base), ("builder", args.builder_base)):
            probe = subprocess.run(["docker", "image", "inspect", ref], capture_output=True)
            if probe.returncode:
                execute(["docker", "pull", ref], "pull-" + role)
            image = json.loads(command("docker", "image", "inspect", ref))[0]
            alias = "local/rcoder-ci-base-" + role + ":" + run_id
            command("docker", "tag", image["Id"], alias)
            bases[role] = {"image_id": image["Id"], "alias": alias, "input": ref}
        dockerfile = folder / "Dockerfile.ci"
        dockerfile.write_text(
            "FROM " + bases["controller"]["alias"] + " AS compile\n"
            "WORKDIR /ci-source\nCOPY . .\n"
            "ENV CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=2\n"
            "RUN cargo build --locked --bin rcoder --bin agent_runner && "
            "cargo build --locked -p rcoder-e2e --bin userapp-db-observer && "
            "CARGO_TARGET_DIR=/ci-appcli-target cargo build --locked --manifest-path crates/app-cli/Cargo.toml --bin app-cli\n"
            "FROM " + bases["controller"]["alias"] + " AS controller\n"
            "COPY --from=compile /ci-source/target/debug/rcoder /app/bin/rcoder\n"
            "COPY --from=compile /ci-source/target/debug/agent_runner /app/bin/agent_runner\n"
            "COPY --from=compile /ci-source/target/debug/userapp-db-observer /app/bin/userapp-db-observer\n"
            "FROM " + bases["builder"]["alias"] + " AS builder\n"
            "COPY --from=compile /ci-source/target/debug/agent_runner /usr/local/bin/agent_runner\n"
            "COPY --from=compile /ci-appcli-target/debug/app-cli /usr/local/bin/app-cli\n")
        images = {}
        for role in ("controller", "builder"):
            tag = "local/rcoder-ci-" + role + ":" + run_id
            execute(["docker", "build", "--pull=false", "--target", role, "-t", tag,
                     "-f", str(dockerfile), str(snapshot)], "build-" + role)
            images[role] = command("docker", "image", "inspect", "--format", "{{.Id}}", tag)
        def binary_sha(role, path):
            return command("docker", "run", "--rm", "--network", "none", "--entrypoint",
                           "sha256sum", images[role], path).split()[0]
        build = {"version": 1, "source": source, "bases": bases,
                 "rcoder": {"image_id": images["controller"],
                            "binary_sha256": binary_sha("controller", "/app/bin/rcoder"),
                            "observer_sha256": binary_sha("controller", "/app/bin/userapp-db-observer")},
                 "builder": {"image_id": images["builder"],
                             "agent_runner_sha256": binary_sha("builder", "/usr/local/bin/agent_runner"),
                             "app_cli_sha256": binary_sha("builder", "/usr/local/bin/app-cli")}}
        validate_build(build, source)
        receipt = folder / "build-receipt.json"
        receipt.write_text(json.dumps(build, indent=2))
        record["build_receipt"] = str(receipt)
        text = isolated_config((snapshot / "docker/config.yml").read_text())
        if "dev-rcoder-agent-runner:latest" not in text:
            raise ValueError("builder image configuration changed")
        config_path = folder / "config.yml"
        config_path.write_text(text.replace("dev-rcoder-agent-runner:latest", images["builder"]))
        config_path.chmod(0o600)
        cfg = service_config(folder, images["controller"], run_id, case_id,
                             os.environ.get("DOCKER_SOCKET_PATH", "/var/run/docker.sock"), config_path)
        service = cfg["services"]["rcoder"]
        service["environment"].update(COMPOSE_PROJECT_NAME=project, DOCKER_NETWORK_BASE_NAME="agent-network")
        service["ports"].append("127.0.0.1::8088")
        service["volumes"] = [m for m in service["volumes"] if m["target"] != "/app/data"]
        service["volumes"].append({"type": "volume", "source": "data", "target": "/app/data"})
        labels = {"rcoder.e2e.run": run_id, "rcoder.e2e.case": case_id}
        cfg["volumes"], cfg["networks"] = {"data": {"labels": labels}}, {"agent-network": {"labels": labels}}
        service["networks"] = ["agent-network"]
        for mount in service["volumes"]:
            if mount["type"] == "bind" and not mount.get("read_only") and mount["target"] != "/var/run/docker.sock":
                Path(mount["source"]).mkdir(parents=True, exist_ok=True)
        (folder / "compose.json").write_text(json.dumps(cfg, indent=2))
        execute(compose + ["up", "-d", "--no-build", "--pull", "never"], "compose-up")
        controller = command(*compose, "ps", "-q", "rcoder")
        record["controller_id"] = controller
        row = json.loads(command("docker", "inspect", controller))[0]
        validate_owned(row, record)
        base = "http://" + command(*compose, "port", "rcoder", "8090")
        proxy = "http://" + command(*compose, "port", "rcoder", "8088")
        wait_ready(base)
        actual_binary = command("docker", "exec", controller, "sha256sum", "/app/bin/rcoder").split()[0]
        if actual_binary != build["rcoder"]["binary_sha256"]:
            raise ValueError("running controller binary differs from compile receipt")
        record["running_binary_sha256"] = actual_binary
        env = dict(os.environ, RCODER_URL=base, E2E_PINGORA_URL=proxy,
                   E2E_SOURCE_ROOT=str(snapshot), E2E_INPUT_MANIFEST=str(manifest_path),
                   E2E_ORIGIN_HEAD=source["origin_head"], E2E_RUN_ROOT=str(folder / "reports"),
                   E2E_OWNER_CONTROLLER_ID=controller, E2E_COMPUTE_BUILD_RECEIPT=str(receipt),
                   E2E_IDLE_RCODER_IMAGE=images["controller"], E2E_IDLE_BUILDER_IMAGE=images["builder"],
                   CARGO_BUILD_JOBS="2", CARGO_TARGET_DIR=str(client_target))
        shim = folder / "python-bin"
        shim.mkdir()
        (shim / "python3").symlink_to(Path(sys.executable).resolve())
        env["PATH"] = str(shim) + os.pathsep + env["PATH"]
        for suite, case in CASES:
            if source_identity(ROOT) != source:
                raise ValueError("source changed before strict case")
            known = set((folder / "reports").glob("*/summary.json"))
            with (folder / (case + ".log")).open("w") as log:
                result = subprocess.run([sys.executable, str(snapshot / "tests-e2e/tools/run.py"),
                                         "--suite", suite, "--filter", case], cwd=ROOT,
                                        env=env, stdout=log, stderr=subprocess.STDOUT)
            reports = set((folder / "reports").glob("*/summary.json")) - known
            if len(reports) != 1:
                raise ValueError("strict case did not produce exactly one summary")
            summary_path = reports.pop()
            summary = json.loads(summary_path.read_text())
            ok = successful_case(result.returncode, summary, case)
            record["scenarios"].append({"suite": suite, "case": case, "success": ok,
                                        "exit_code": result.returncode, "summary": str(summary_path)})
            save()
        record["cases_passed"] = all(row["success"] for row in record["scenarios"]) and len(record["scenarios"]) == len(CASES)
        if source_identity(ROOT) != source:
            raise ValueError("source changed during strict cases")
    except Exception as error:
        record["error"] = type(error).__name__ + ": " + str(error)
        record["success"] = False
    finally:
        finish_owned_run(record, folder, compose, controller, before, source, execute, save)
    print(json.dumps({"report": str(folder / "result.json"), "success": record["success"]}))
    return 0 if record["success"] and record.get("cleanup_confirmed") and record["preexisting_containers_preserved"] else 1


if __name__ == "__main__":
    sys.exit(main())
