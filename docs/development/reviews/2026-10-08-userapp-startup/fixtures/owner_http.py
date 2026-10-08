#!/usr/bin/env python3
"""Build an independent HTTP review fixture against the actual file-server crate.

Default mode records the bad baseline behavior; --assert-correct fails until the
reported contracts are fixed. This is a component fixture, not a container E2E.
Only a new explicitly supplied work directory is written. Cargo tasks are serial.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tomllib


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, required=True,
                        help="new fixture directory on the designated work disk")
    parser.add_argument("--assert-correct", action="store_true",
                        help="assert corrected product behavior, instead of observing")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("this fixture requires a POSIX shell and executable permissions")
    repo = Path(__file__).resolve().parents[5]
    source = Path(__file__).with_name("owner_http.rs")
    if not (repo / "crates/file-server/Cargo.toml").is_file():
        parser.error("run the fixture from its location inside the rcoder repository")
    work = args.work_dir.expanduser().absolute()
    try:
        work.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error("--work-dir already exists; select a new owned directory")
    work = work.resolve()
    temporary = work / "tmp"
    temporary.mkdir()
    crate = work / "fixture"
    crate.mkdir()
    shutil.copy2(source, crate / "main.rs")
    lock = tomllib.loads((repo / "Cargo.lock").read_text())
    workspace_deps = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["dependencies"]
    lines = ["[package]", 'name = "owner-http-review"', 'version = "0.0.0"',
             'edition = "2024"', "[workspace]", "[dependencies]",
             "file-server = { path = " + json.dumps(str(repo / "crates/file-server")) + " }",
             "shared_types = { path = " + json.dumps(str(repo / "crates/shared_types")) + " }"]
    for name in ("anyhow", "axum", "serde_json", "tokio", "async-trait", "reqwest", "tempfile"):
        declaration = workspace_deps[name]
        options = dict(declaration) if isinstance(declaration, dict) else {"version": declaration}
        options["version"] = "=" + next(item["version"] for item in lock["package"] if item["name"] == name)
        fields = ", ".join(key + " = " + json.dumps(value) for key, value in options.items())
        lines.append(name + " = { " + fields + " }")
    lines.extend(["[[bin]]", 'name = "owner-http-review"', 'path = "main.rs"'])
    (crate / "Cargo.toml").write_text("\n".join(lines) + "\n")
    shutil.copy2(repo / "Cargo.lock", crate / "Cargo.lock")
    env = os.environ.copy()
    env.pop("PROJECT_ID", None)
    env.pop("APP_CLI_STATE_ROOT", None)
    env["TMPDIR"] = str(temporary)
    env["RCODER_REVIEW_WORK_DIR"] = str(work)
    # Reuse the caller's configured work-disk target; otherwise use the repo's
    # target on the same checkout disk. Do not alter global Cargo/rustup settings.
    env.setdefault("CARGO_TARGET_DIR", str(repo / "target"))
    command = ["cargo", "run", "--manifest-path", str(crate / "Cargo.toml"),
               "--bin", "owner-http-review"]
    if args.offline:
        command.append("--offline")
    command.append("--")
    if not args.assert_correct:
        command.append("--observe")
    result = subprocess.run(command, env=env, cwd=repo, capture_output=True, text=True)
    (work / "stdout.log").write_text(result.stdout)
    (work / "stderr.log").write_text(result.stderr)
    (work / "exit-code.txt").write_text(str(result.returncode) + "\n")
    print(result.stdout, end="")
    if result.returncode:
        print(result.stderr[-6000:], end="", file=__import__("sys").stderr)
    print(f"fixture evidence retained in {work}")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
