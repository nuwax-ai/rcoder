#!/usr/bin/env python3
"""闲置恢复工具的目录与收据契约单测；不执行 Docker/Compose 验收。"""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    'idle_owner_recovery', Path(__file__).with_name('idle_owner_recovery.py'))
TOOL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TOOL)


APP = 'idle0123456789abcdef'
SOURCE = '/home/user/' + APP
STATE = SOURCE + '/state/' + APP
GENERATION = '39aa560f-5aba-43b6-a504-b13928e79f54'


class IdleOwnerRecoveryToolTests(unittest.TestCase):
    def test_real_fixture_config_uses_native_managed_state_and_three_second_stop(self):
        original = (TOOL.REPO / 'docker/config.yml').read_text()
        configured = TOOL.idle_config(original, 'sha256:fixture-image', APP, SOURCE)
        self.assertIn('  docker_stop_timeout_seconds: 3\n', configured)
        self.assertIn('          APP_CLI_STATE_ROOT: ' + json.dumps(STATE) + '\n', configured)
        self.assertIn('          APP_CLI_RUNTIME_WORKSPACE: ' + json.dumps(SOURCE) + '\n', configured)
        self.assertEqual(configured.count('          APP_CLI_STATE_ROOT:'), 1)
        self.assertEqual(configured.count('          APP_CLI_RUNTIME_WORKSPACE:'), 1)
        self.assertNotIn('/home/user/logs/.app-cli-state', configured)
        self.assertEqual(TOOL.IDLE_SECONDS, 60)
        self.assertEqual(TOOL.SCAN_SECONDS, 5)
        with self.assertRaisesRegex(ValueError, 'already declares'):
            TOOL.idle_config(configured.replace('sha256:fixture-image',
                             'dev-rcoder-agent-runner:latest'), 'sha256:new-image', APP, SOURCE)

    def test_source_scope_is_application_specific_and_never_uses_code_or_logs(self):
        self.assertEqual(TOOL.fixture_layout(APP, SOURCE), {
            'source_root': SOURCE, 'state_root': STATE})
        for bad_app in ('', '../foreign', '..', 'wrong/application', '/foreign', 'has space'):
            with self.assertRaises(ValueError):
                TOOL.fixture_layout(bad_app, SOURCE)
        for bad_source in ('/home/user/another-app', SOURCE + '/code',
                           '/home/user/logs', SOURCE + '/../foreign', SOURCE + '/state'):
            with self.assertRaises(ValueError):
                TOOL.fixture_layout(APP, bad_source)

    def test_retained_scope_is_inside_exact_source_mount_and_owned_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = Path(directory).resolve() / 'userapp-workspace'
            host_source = owned / APP
            host_source.mkdir(parents=True)
            mounts = [{'type': 'bind', 'source': str(host_source),
                       'destination': SOURCE, 'read_only': False},
                      {'type': 'bind', 'source': str(Path(directory) / 'logs'),
                       'destination': '/home/user/logs', 'read_only': False}]
            expected = host_source / 'state' / APP
            self.assertEqual(TOOL.retained_state_root(mounts, APP, SOURCE, owned), expected)
            for invalid in ([mounts[1]], mounts + [mounts[0]],
                            [dict(mounts[0], type='volume')],
                            [dict(mounts[0], read_only=True)],
                            [dict(mounts[0], source=str(Path(directory) / 'foreign'))],
                            [dict(mounts[0], destination='/home/user/another-app')]):
                with self.assertRaises(ValueError):
                    TOOL.retained_state_root(invalid, APP, SOURCE, owned)
            outside = Path(directory) / 'outside'
            outside.mkdir()
            (host_source / 'state').symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, 'symlink'):
                TOOL.retained_state_root(mounts, APP, SOURCE, owned)

    def test_capture_verifies_actual_environment_and_rejects_duplicates_or_foreign_root(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = Path(directory).resolve() / 'userapp-workspace'
            host_source = owned / APP
            host_source.mkdir(parents=True)
            mounts = [{'type': 'bind', 'source': str(host_source),
                       'destination': SOURCE, 'read_only': False}]
            environment = ['PROJECT_ID=' + APP, 'USERAPP_WORKSPACE_DIR=/home/user',
                           'APP_CLI_RUNTIME_WORKSPACE=' + SOURCE, 'APP_CLI_STATE_ROOT=' + STATE]
            result = TOOL.verify_builder_layout(environment, mounts, APP, SOURCE, owned)
            self.assertEqual(result['state_root'], STATE)
            self.assertEqual(result['retained_state_root'], str(host_source / 'state' / APP))
            for invalid in (environment[:-1], environment + [environment[-1]],
                            environment[:-1] + ['APP_CLI_STATE_ROOT=/home/user/logs/.app-cli-state'],
                            ['PROJECT_ID=another-app'] + environment[1:]):
                with self.assertRaisesRegex(ValueError, 'declaration differs'):
                    TOOL.verify_builder_layout(invalid, mounts, APP, SOURCE, owned)

    def test_generation_paths_are_pinned_to_app_and_canonical_generation(self):
        expected = STATE + '/work/' + GENERATION + '/generation.json'
        self.assertEqual(TOOL.generation_file(STATE, APP, SOURCE, GENERATION,
                                            'generation.json'), expected)
        self.assertEqual(TOOL.generation_file(STATE, APP, SOURCE, GENERATION,
                          'physical-exit.json'), expected.replace('generation.json', 'physical-exit.json'))
        for state, generation, filename in (
                ('/home/user/another-app/state/another-app', GENERATION, 'generation.json'),
                (STATE, '../foreign', 'generation.json'),
                (STATE, GENERATION, '../supervisor.json'),
                (STATE, GENERATION.upper(), 'generation.json')):
            with self.assertRaises(ValueError):
                TOOL.generation_file(state, APP, SOURCE, generation, filename)

    def test_physical_exit_must_match_all_captured_identity_fields(self):
        captured = {'generation': GENERATION, 'supervisor_id': 'owner-original',
                    'domain': {'authority': 'docker', 'volume': 'volume-original',
                               'instance': 'container-original'},
                    'binding': {'component': 'app-cli', 'resource': SOURCE}}
        proof = json.loads(json.dumps(captured))
        self.assertTrue(TOOL.physical_exit_matches(proof, captured, APP, SOURCE))
        for key, replacement in (
                ('generation', 'another-generation'), ('supervisor_id', 'another-owner'),
                ('domain', dict(captured['domain'], instance='another-container')),
                ('binding', {'component': 'app-cli', 'resource': '/home/user/another-app'})):
            invalid = dict(proof, **{key: replacement})
            self.assertFalse(TOOL.physical_exit_matches(invalid, captured, APP, SOURCE))
            missing = dict(proof)
            missing.pop(key)
            self.assertFalse(TOOL.physical_exit_matches(missing, captured, APP, SOURCE))
        foreign = dict(captured, binding={'component': 'app-cli', 'resource': '/home/user/another-app'})
        self.assertFalse(TOOL.physical_exit_matches(foreign, foreign, APP, SOURCE))
        self.assertFalse(TOOL.physical_exit_matches(dict(proof, domain=None),
                         dict(captured, domain=None), APP, SOURCE))

    def test_fault_script_is_valid_python_and_scope_is_explicitly_checked(self):
        compile(TOOL.OWNER_FAULT_SCRIPT, '<owner-fault>', 'exec')
        self.assertIn("scope, app_id = pathlib.Path(sys.argv[3]).resolve(), sys.argv[4]",
                      TOOL.OWNER_FAULT_SCRIPT)
        self.assertIn("scope == workspace/'state'/app_id", TOOL.OWNER_FAULT_SCRIPT)
        self.assertIn('signal.pidfd_send_signal', TOOL.OWNER_FAULT_SCRIPT)
        self.assertNotIn('/home/user/logs/.app-cli-state', TOOL.OWNER_FAULT_SCRIPT)


if __name__ == '__main__':
    unittest.main()
