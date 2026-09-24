#!/usr/bin/env python3
"""Run the small, explicit Kani proof allowlist with bounded wall-clock time."""

from __future__ import annotations

import os
import queue
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path


KANI_VERSION = "0.68.0"
CBMC_VERSION = "6.11.0"
PROOFS = (
    (
        "shared_types",
        "pg_utils::kani_proofs::pg_identifier_symbolic_grammar",
        90,
        300,
    ),
    (
        "shared_types",
        "pg_utils::kani_proofs::pg_identifier_symbolic_length",
        30,
        120,
    ),
    (
        "shared_types",
        "validation::kani_proofs::identifier_symbolic_grammar",
        90,
        300,
    ),
    (
        "shared_types",
        "validation::kani_proofs::identifier_symbolic_length",
        30,
        120,
    ),
    (
        "shared_types",
        "userapp::lifecycle::kani_progress_claim_proofs::progress_claim_matches_state_contract",
        30,
        120,
    ),
    (
        "shared_types",
        "userapp::lifecycle::kani_scope_conflict_proofs::scope_conflicts_match_contract_and_are_symmetric",
        30,
        120,
    ),
)


def terminate_process_tree(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        process.wait()
    except ProcessLookupError:
        pass


def run_bounded(command: list[str], timeout_seconds: int, cwd: Path) -> tuple[int | None, str, bool]:
    """Stream a command's output while enforcing a wall-clock deadline."""
    try:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
            start_new_session=(os.name == "posix"),
        )
    except OSError as error:
        return None, str(error), False

    lines: list[str] = []
    output_queue: queue.Queue[str | None] = queue.Queue()

    def read_output() -> None:
        assert process.stdout is not None
        for line in process.stdout:
            lines.append(line)
            output_queue.put(line)
        output_queue.put(None)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    deadline = time.monotonic() + timeout_seconds
    timed_out = False
    reader_finished = False

    while not reader_finished:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            timed_out = True
            terminate_process_tree(process)
            break
        try:
            line = output_queue.get(timeout=min(remaining, 0.5))
        except queue.Empty:
            if process.poll() is not None and not reader.is_alive():
                break
            continue
        if line is None:
            reader_finished = True
        else:
            print(line, end="", flush=True)

    try:
        return_code = process.wait(timeout=max(0.0, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        timed_out = True
        terminate_process_tree(process)
        return_code = process.wait()
    reader.join(timeout=3)
    return return_code, "".join(lines), timed_out


def classify_result(
    harness: str, return_code: int | None, output: str, timed_out: bool, spawn_error: str
) -> tuple[str, str]:
    if spawn_error:
        return "NOT_RUN", spawn_error
    if timed_out:
        return "TIMEOUT", "proof exceeded its wall-clock limit"
    folded = output.casefold()
    failed_checks = re.search(r"^failed checks:\s*(.+)$", folded, flags=re.MULTILINE)
    if failed_checks:
        if "unwinding assertion" in failed_checks.group(1):
            return "UNWINDING", "Kani reported a failed unwinding assertion"
        return "FAILURE", f"Kani reported a failed check: {failed_checks.group(1)}"
    if "one or more unwinding failures" in folded:
        return "UNWINDING", "Kani reported a failed unwinding assertion"
    if "timed out" in folded or "timeout" in folded:
        return "TIMEOUT", "Kani/CBMC exceeded the proof time limit"
    if (
        "verification:- undetermined" in folded
        or re.search(r"- status: undetermined\b", folded)
        or re.search(r"\([1-9]\d*\s+undetermined\b", folded)
    ):
        return "UNDETERMINED", "Kani/CBMC could not decide the proof"
    if harness.rsplit("::", 1)[-1] not in output:
        return "NOT_RUN", "Kani output did not show the selected harness"
    if return_code == 0 and "verification:- successful" in folded:
        return "PASS", "proved"
    if "verification:- failed" in folded or return_code != 0:
        return "FAILURE", f"Kani exited with status {return_code}"
    if "verification:- successful" not in folded:
        return "NOT_RUN", "Kani did not report a successful verification summary"
    return "FAILURE", "Kani reported an inconsistent proof result"


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    version_command = ["cargo", "kani", "--version"]
    try:
        version_result = subprocess.run(
            version_command,
            cwd=root,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        version_result = None
        version_output = str(error)
    else:
        version_output = version_result.stdout + version_result.stderr

    if (
        version_result is None
        or version_result.returncode != 0
        or f"Kani Rust Verifier {KANI_VERSION}" not in version_output
        or f"CBMC {CBMC_VERSION}" not in version_output
    ):
        print(
            f"TOOLCHAIN_ERROR: expected Kani {KANI_VERSION} with CBMC {CBMC_VERSION}; "
            f"found: {version_output.strip() or 'no version output'}",
            file=sys.stderr,
        )
        for package, harness, proof_timeout_seconds, _ in PROOFS:
            print(
                f"NOT_RUN {package}::{harness} ({proof_timeout_seconds}s proof limit): "
                "toolchain version check failed"
            )
        return 1

    print(f"Kani formal gate: Kani {KANI_VERSION}, CBMC {CBMC_VERSION}")
    results: list[tuple[str, str, str]] = []
    for package, harness, proof_timeout_seconds, process_timeout_seconds in PROOFS:
        print(
            f"\n=== {package}::{harness} "
            f"(proof timeout {proof_timeout_seconds}s; process cap {process_timeout_seconds}s) ===",
            flush=True,
        )
        command = [
            "cargo",
            "kani",
            "-p",
            package,
            "--exact",
            "--harness",
            harness,
            "-Z",
            "unstable-options",
            "--harness-timeout",
            f"{proof_timeout_seconds}s",
            "--output-format",
            "terse",
        ]
        return_code, output, timed_out = run_bounded(command, process_timeout_seconds, root)
        spawn_error = output if return_code is None else ""
        status, reason = classify_result(harness, return_code, output, timed_out, spawn_error)
        results.append((package, harness, status))
        print(f"RESULT {status}: {package}::{harness} — {reason}")

    print("\nKani proof summary:")
    for package, harness, status in results:
        print(f"  {status:12} {package}::{harness}")

    failures = [result for result in results if result[2] != "PASS"]
    if failures:
        print("Kani formal gate FAILED; only PASS counts as proved.", file=sys.stderr)
        return 1
    print(f"Kani formal gate PASSED ({len(results)} explicit proofs).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
