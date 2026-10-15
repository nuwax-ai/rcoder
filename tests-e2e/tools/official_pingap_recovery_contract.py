#!/usr/bin/env python3
"""Strict owned Docker acceptance for withdrawn official-proxy recovery.

Write a source receipt before building the Linux app-cli, register that exact
binary after the build, then run. This entrance needs no Compose/RCoder/LLM and
never changes an existing application, cluster, owner journal, or volume.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid

from hot_cleanup import run_owned
from isolated_docker import require_local_docker_endpoint
from userapp_root_logs import source_snapshot

REPO = Path(__file__).resolve().parents[2]
TOOLS = Path(__file__).parent
SCHEMA = "official-proxy-recovery-v1"
REQUIRED = {"fresh app-cli binary matches the build source receipt": 1,
            "unmodified official 0.15.0 protocol": 1,
            "original native StopWork confirms physical cleanup": 2,
            "original native StopWork restores management after business exit": 2,
            "late Stop leaves the real proxy STOPPED with PID zero and Remove pending": 1,
            "late Remove already removed the real group before its reply": 1,
            "both delayed faults preserve owned workspace data": 1,
            "real supervisor transport has no injection failures": 1,
            "transport shutdown drains accepted RPC handlers": 1}
for method in ("stopProcessGroup", "removeProcessGroup"):
    for suffix in (" fault actually held a completed real supervisor RPC",
                   " expired caller leaves proxy configuration withdrawn",
                   " recovery retains the original management owner",
                   " management initialization completes before original Restart",
                   " original Restart restores one current business process",
                   " recovered real supervisor has one owner, proxy, and business",
                   " recovery preserves the original failed operation",
                   " original failed operation has a durable receipt",
                   " recovery preserves the original private receipt and request"):
        REQUIRED[method + suffix] = 1


def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def harness():
    return {name: sha(TOOLS / name) for name in
            ("official_pingap_lifecycle.py", "official_pingap_faults.py", Path(__file__).name)}


def validate_build(receipt, binary):
    if receipt.get("schema") != SCHEMA:
        raise ValueError("missing or unsupported official-proxy source receipt")
    if receipt.get("source", {}).get("source_inputs_sha256") != source_snapshot(REPO)["source_inputs_sha256"]:
        raise ValueError("source changed after the Linux app-cli build was recorded")
    if receipt.get("harness_sha256") != harness():
        raise ValueError("acceptance harness changed after the build receipt")
    if receipt.get("binary_sha256") != sha(binary):
        raise ValueError("app-cli binary differs from the registered build artifact")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app-cli", type=Path)
    parser.add_argument("--build-source", type=Path)
    parser.add_argument("--write-build-source", type=Path)
    parser.add_argument("--register-binary", action="store_true")
    parser.add_argument("--image", default="dev-app-runtime:latest")
    parser.add_argument("--platform", default="linux/amd64")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if args.write_build_source:
        args.write_build_source.parent.mkdir(parents=True, exist_ok=True)
        args.write_build_source.write_text(json.dumps({"schema": SCHEMA,
            "source": source_snapshot(REPO), "harness_sha256": harness()}, indent=2) + "\n")
        return 0
    if not args.app_cli or not args.build_source:
        parser.error("--app-cli and --build-source are required; record source before building")
    args.app_cli = args.app_cli.resolve(strict=True)
    receipt = json.loads(args.build_source.read_text())
    if args.register_binary:
        provisional = dict(receipt, binary_sha256=sha(args.app_cli))
        validate_build(provisional, args.app_cli)
        args.build_source.write_text(json.dumps(provisional, indent=2) + "\n")
        return 0
    if not args.report:
        parser.error("--report is required for acceptance")
    validate_build(receipt, args.app_cli)
    endpoint = require_local_docker_endpoint()
    run_id = uuid.uuid4().hex
    report = args.report.resolve()
    report.mkdir(parents=True, exist_ok=False)
    state = report / "state"
    workspace = report / "workspace"
    state.mkdir(); workspace.mkdir()
    summary = {"schema": SCHEMA, "run_id": run_id, "verdict": "failed", "build": receipt,
               "required": REQUIRED, "platform": args.platform, "cleanup": {"completed": False}}
    def save():
        (report / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    def docker(*argv, timeout=90):
        result = subprocess.run(["docker", "--host", endpoint, *argv], capture_output=True,
                                text=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError("Docker " + (argv[0] if argv else "command")
                               + " failed with exit " + str(result.returncode))
        return result.stdout.strip()
    cid = None
    save()
    try:
        images = json.loads(docker("image", "inspect", args.image))
        if len(images) != 1 or not re.fullmatch(r"sha256:[0-9a-f]{64}", images[0]["Id"]):
            raise ValueError("runtime image has no immutable local identity")
        image = images[0]
        summary["image"] = {key: image.get(key) for key in ("Id", "RepoDigests", "Os", "Architecture")}
        summary["containers_before"] = docker("ps", "-aq", "--no-trunc").split()
        save()
        cid = run_owned(report, run_id, run_id, ["-d", "--network", "none", "--platform", args.platform,
            "--mount", "type=bind,src=" + str(TOOLS.resolve()) + ",dst=/opt/contract,readonly",
            "--mount", "type=bind,src=" + str(args.app_cli) + ",dst=/usr/local/bin/app-cli,readonly",
            "--mount", "type=bind,src=" + str(state) + ",dst=/state",
            "--mount", "type=bind,src=" + str(workspace) + ",dst=/workspace",
            "--entrypoint", "python3", image["Id"], "/opt/contract/official_pingap_faults.py",
            "--source-digest", receipt["source"]["source_inputs_sha256"],
            "--binary-sha256", receipt["binary_sha256"]], "supervisord", docker_fn=docker)
        summary["container_id"] = cid
        save()
        # Each fault consumes the real 25s lifecycle deadline; the whole budget
        # still bounds a hung test. Docker wait observes only this captured ID.
        exit_code = int(docker("wait", cid, timeout=300))
        summary["container_exit_code"] = exit_code
        evidence = json.loads((state / "evidence.json").read_text())
        summary["evidence"] = "state/evidence.json"
        counts = Counter(row["name"] for row in evidence.get("checks", []) if row.get("passed") is True)
        summary["missing"] = {name: count - counts[name] for name, count in REQUIRED.items()
                              if counts[name] < count}
        summary["checks_passed"] = (bool(evidence.get("checks"))
            and all(row.get("passed") is True for row in evidence["checks"]))
        summary["source_unchanged"] = receipt["source"]["source_inputs_sha256"] == source_snapshot(REPO)["source_inputs_sha256"]
        summary["binary_unchanged"] = receipt["binary_sha256"] == sha(args.app_cli)
        summary["harness_unchanged"] = receipt["harness_sha256"] == harness()
        if exit_code == 0 and evidence.get("passed") is True and summary["checks_passed"] \
                and not summary["missing"] and summary["source_unchanged"] and summary["binary_unchanged"] and summary["harness_unchanged"]:
            summary["verdict"] = "passed"
    except Exception as error:
        summary["error"] = str(error)
    finally:
        try:
            ownership = json.loads((report / "ownership.json").read_text())
            token = ownership["ownership_token"]
            ids = docker("ps", "-aq", "--no-trunc", "--filter", "name=^/" + ownership["name"] + "$").split()
            if len(ids) != 1:
                raise ValueError("the captured owned container is missing or ambiguous")
            info = json.loads(docker("inspect", ids[0]))[0]
            labels = info["Config"].get("Labels") or {}
            if (info["Id"] != cid or cid in summary.get("containers_before", [])
                    or info["Name"] != "/rcoder-review-" + token
                    or labels.get("rcoder.e2e.run") != run_id or labels.get("rcoder.e2e.case") != run_id
                    or labels.get("rcoder.e2e.hot") != token):
                raise ValueError("owned container identity changed; cleanup refused")
            (report / "container.log").write_text(docker("logs", cid))
            docker("rm", "-f", cid)
            summary["cleanup"] = {"completed": True, "container_id": cid,
                                  "workspace_retained": workspace.is_dir(), "volumes_removed": False}
        except Exception as error:
            summary["cleanup"]["error"] = str(error)
            summary["verdict"] = "failed"
        save()
    print(json.dumps({"verdict": summary["verdict"], "report": str(report),
                      "error": summary.get("error"), "cleanup": summary["cleanup"]}, ensure_ascii=False))
    return 0 if summary["verdict"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
