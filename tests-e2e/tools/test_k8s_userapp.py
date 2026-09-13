"""Local safety/report gates for the real-cluster launcher; no external I/O."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

import k8s_userapp as acceptance


class ReportTests(unittest.TestCase):
    def report(self, root):
        report = acceptance.Run.__new__(acceptance.Run)
        report.root = Path(root)
        report.report_lock = threading.RLock()
        report.assertions = []
        return report

    def test_resource_evidence_excludes_pod_environment(self):
        evidence = acceptance.compact_resource({
            'kind': 'Pod', 'metadata': {'name': 'app', 'uid': 'uid'},
            'spec': {'containers': [{'env': [{'name': 'TOKEN', 'value': 'private'}]}]},
            'status': {'containerStatuses': [{'name': 'app', 'imageID': 'sha256:actual', 'ready': True}]},
        })
        self.assertNotIn('private', json.dumps(evidence))
        self.assertEqual(evidence['images'][0]['imageID'], 'sha256:actual')

    def test_reports_redact_nested_credentials(self):
        with tempfile.TemporaryDirectory() as root:
            report = self.report(root)
            report.save('record.json', {'data': {'env': {'APP_CLI_TOKEN': 'private', 'PORT': '3010'}, 'password': 'private'}})
            data = json.loads((Path(root) / 'record.json').read_text())
            self.assertNotIn('private', json.dumps(data))
            self.assertEqual(data['data']['env']['PORT'], '3010')

    def test_nonfatal_failure_is_retained_while_later_steps_continue(self):
        with tempfile.TemporaryDirectory() as root, contextlib.redirect_stdout(io.StringIO()):
            report = self.report(root)
            report.check('concurrent_ensure', False, fatal=False)
            report.check('ensure_retry', True)
            rows = json.loads((Path(root) / 'assertions.json').read_text())
            self.assertEqual([row['ok'] for row in rows], [False, True])

    def test_empty_scenario_cannot_pass(self):
        class EmptyRun(acceptance.Run):
            def local(self, *_args):
                return 'test-head'
            def prepare(self):
                pass
            def workspace(self):
                pass
            def build(self, _version):
                return {}, ''
            def deploy(self, *_args):
                pass
            def cleanup(self):
                self.check('cleanup', True)
        with tempfile.TemporaryDirectory() as root, \
                patch.object(acceptance, 'ROOT', Path(root)), \
                patch.object(acceptance, 'Run', EmptyRun), \
                patch('run.source_fingerprint', return_value='test-fingerprint'), \
                patch('signal.signal'), \
                patch('sys.argv', ['k8s_userapp.py', '--ssh', 'test', '--url', 'http://test', '--proxy-url', 'http://test']), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(acceptance.main(), 1)
            summary = json.loads(next(Path(root).glob('reports/*/summary.json')).read_text())
            self.assertEqual(summary['verdict'], 'fail')
            self.assertIn('cold_deploy', summary['missing'])


if __name__ == '__main__':
    unittest.main()
