"""Validate Make failure propagation without touching a Docker daemon."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]

class RestartTests(unittest.TestCase):
    def run_restart(self, failure):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'docker').mkdir()
            (root / 'docker/docker-compose.yml').touch()
            stub = root / 'docker-compose'
            stub.write_text('#!/bin/sh\necho "$*" >> "$CALL_LOG"\ncase "$*" in\n  *"' + failure + '"*) exit 42;;\nesac\n')
            stub.chmod(0o755)
            makefile = root / 'Makefile'
            makefile.write_text('include ' + str(REPO / 'make/dev.mk') + '\ndocker-build: ; @true\ndev-build: ; @true\n')
            log = root / 'calls'
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'], CALL_LOG=str(log))
            result = subprocess.run(['make', 'dev-restart'], cwd=root, env=env, capture_output=True, text=True)
            return result, log.read_text()

    def test_down_failure_prevents_start_and_success(self):
        result, calls = self.run_restart('down')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('up -d', calls)
        self.assertNotIn('完整重启完成', result.stdout)

    def test_up_failure_is_not_success(self):
        result, _ = self.run_restart('up -d')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('完整重启完成', result.stdout)
