"""Source fixture inputs only; real two-engine acceptance runs separately."""
import os
from pathlib import Path
import re
import runpy
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch


class WorkerSourceInputTests(unittest.TestCase):
    def test_source_declares_actual_workspace_projects_and_runs_current_source(self):
        with tempfile.TemporaryDirectory() as report, patch.dict(
                os.environ, {'E2E_REPORT_DIR': report, 'E2E_RUN_ID': 'worker-source-inputs'}), \
                patch.dict(sys.modules):
            sys.modules.pop('hot_contract', None)
            module = runpy.run_path(str(Path(__file__).with_name('worker_contract.py')))
            files = module['source_files']()
        workspace = tomllib.loads(files['workspace.manifest.toml'])
        self.assertEqual(workspace['schema_version'], 1)
        self.assertNotIn('release.lock.toml', files)
        self.assertNotIn('web/dependency.js', files, 'deployed real symlink must remain intact')
        types = (Path(__file__).resolve().parents[2] / 'crates/workspace-manifest/src/types.rs').read_text()
        def fields(name):
            declaration = re.search(r'pub struct ' + name + r'\s*\{(.*?)\n\}', types, re.S)
            self.assertIsNotNone(declaration)
            return set(re.findall(r'^\s*pub (?:r#)?(\w+)\s*:', declaration.group(1), re.M))
        self.assertLessEqual(set(workspace), fields('WorkspaceManifest'))
        for name, probe in [('web', 'http'), ('worker', 'process')]:
            project = tomllib.loads(files[name + '/project.manifest.toml'])
            self.assertLessEqual(set(project), fields('ProjectManifest'))
            self.assertLessEqual(set(project['project']), fields('ProjectMeta'))
            self.assertEqual(project['project']['service_id'], name)
            self.assertEqual(project['health']['startup_probe'], probe)
            self.assertEqual(project['run']['command'], project['devrun']['command'])
            self.assertGreater(project['run']['shutdown_timeout_seconds'], 0)
        self.assertEqual(tomllib.loads(files['worker/project.manifest.toml'])['project']['kind'], 'worker')
        compile(files['worker/main.py'], 'worker/main.py', 'exec')
        self.assertIn('Number(process.env.PORT)', files['web/server.js'])
        self.assertIn("require('./dependency.js')", files['web/server.js'])


if __name__ == '__main__':
    unittest.main()
