"""Run the real aggregate Make recipe with isolated recording child targets."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]


class DockerBuildTests(unittest.TestCase):
    def run_build(self, failure=''):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scripts = root / 'k8s/scripts'
            scripts.mkdir(parents=True)
            # The aggregate recipe now gates Pingap before either build. This
            # fixture records that subprocess boundary; it does not invoke Docker.
            (scripts / 'pingap_version_gate.py').write_text(
                'import os, sys\n'
                'with open(os.environ["CALL_LOG"], "a") as log:\n'
                '    log.write("gate\\n")\n'
                'sys.exit(1 if os.environ["FAIL_TARGET"] == "gate" else 0)\n'
            )
            (root / 'Makefile').write_text(
                'include ' + str(REPO / 'make/docker.mk') + '\n'
                '.PHONY: docker-build-agent-runner docker-build-master\n'
                'docker-build-agent-runner:\n'
                '\t@echo agent-start >> "$(CALL_LOG)"\n'
                '\t@test "$(FAIL_TARGET)" != agent\n'
                '\t@echo agent-complete >> "$(CALL_LOG)"\n'
                'docker-build-master:\n'
                '\t@echo master-start >> "$(CALL_LOG)"\n'
                '\t@test "$(FAIL_TARGET)" != master\n'
                '\t@echo master-complete >> "$(CALL_LOG)"\n'
            )
            log = root / 'calls'
            env = dict(os.environ, CALL_LOG=str(log), FAIL_TARGET=failure)
            result = subprocess.run(['make', '-j4', 'docker-build'], cwd=root, env=env, capture_output=True, text=True, timeout=20)
            self.assertTrue(log.exists(), result.stdout + result.stderr)
            return result, log.read_text().splitlines()

    def test_gate_failure_prevents_both_builds(self):
        result, calls = self.run_build('gate')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, ['gate'])
        self.assertNotIn('所有 Docker 镜像构建完成', result.stdout)

    def test_agent_failure_never_starts_master(self):
        result, calls = self.run_build('agent')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, ['gate', 'agent-start'])
        self.assertNotIn('所有 Docker 镜像构建完成', result.stdout)

    def test_master_failure_never_claims_success(self):
        result, calls = self.run_build('master')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, ['gate', 'agent-start', 'agent-complete', 'master-start'])
        self.assertNotIn('所有 Docker 镜像构建完成', result.stdout)

    def test_parallel_make_still_completes_agent_before_master(self):
        result, calls = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, ['gate', 'agent-start', 'agent-complete', 'master-start', 'master-complete'])
        self.assertIn('dev-rcoder-agent-runner:latest', result.stdout)
        self.assertNotIn('dev-computer-agent-runner', result.stdout)


if __name__ == '__main__':
    unittest.main()
