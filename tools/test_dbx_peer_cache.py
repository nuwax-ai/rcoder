#!/usr/bin/env python3
"""Explicit cross-repository DBX protocol gate (local builds need no peer repo)."""
import argparse
import importlib.util
from pathlib import Path
import subprocess
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('dbx_protocol_tests', HERE / 'tests/test_dbx_cache.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class PeerCacheTests(fixture.CacheProtocolTests):
    def test_default_cache_is_workspace_shared_without_cwd_authority(self):
        observed = []
        for makefile, repository in [(fixture.REPO / 'make/dbx.mk', fixture.REPO),
                                     (self.peer / 'makefiles/16-app-runtime.mk', self.peer)]:
            command = ['make', '-f', str(makefile), '-f', '-', 'show-cache-root', f'PROJECT_ROOT={self.peer}']
            result = subprocess.run(command, input='show-cache-root:\n\t@printf "%s\\n" "$(DBX_PERSIST_ROOT)"\n',
                                    cwd=self.root, env=self.env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            actual = Path(result.stdout.strip())
            self.assertEqual(actual, repository.parent / '.cache/nuwax-build/dbx')
            observed.append(actual)
        if self.peer.parent == fixture.REPO.parent:
            self.assertEqual(observed[0], observed[1])

    def test_peer_make_concurrent_cache_reuse(self):
        peer = self.peer
        self.assertEqual(fixture.HELPER.read_bytes(), (peer / 'scripts/build/dbx_cache.py').read_bytes(),
                         'DBX v2 helpers must remain byte-identical')
        reference2 = self.root / 'peer-request.json'
        context2 = self.root / 'peer/downloads'
        common = [f'DBX_PERSIST_ROOT={self.cache}', f'DBX_FORK_REPO={self.source}',
                  f'DBX_FORK_BRANCH={self.branch}', 'DBX_BUILDER=fixture']
        local_command = ['make', '-f', str(fixture.REPO / 'make/dbx.mk'), 'build-dbx-fork',
                         *common, f'DBX_CONTEXTS={self.context}', f'DBX_OUTPUT_REF={self.ref}']
        peer_command = ['make', '-f', str(peer / 'makefiles/16-app-runtime.mk'), 'build-dbx-fork',
                        *common, f'PROJECT_ROOT={peer}', 'BUILDX_LOCAL_BUILDER=fixture',
                        f'DBX_CONTEXTS={context2}', f'DBX_OUTPUT_REF={reference2}']
        environment = {**self.env, 'BUILD_DELAY': '0.2'}
        local = subprocess.Popen(local_command, cwd=self.root, env=environment,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        remote = subprocess.Popen(peer_command, cwd=self.root, env=environment,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        local_output = local.communicate(timeout=30)
        peer_output = remote.communicate(timeout=30)
        self.assertEqual(local.returncode, 0, local_output)
        self.assertEqual(remote.returncode, 0, peer_output)
        self.assertEqual(self.entry(), self.entry(reference2))
        self.assertEqual(len(self.builds()), 2)
        self.assertEqual((context2 / 'dbx-web-amd64').read_bytes(),
                         (self.context / 'dbx-web-amd64').read_bytes())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--peer-root', type=Path, required=True)
    arguments = parser.parse_args()
    PeerCacheTests.peer = arguments.peer_root.resolve()
    suite = unittest.TestSuite([PeerCacheTests('test_peer_make_concurrent_cache_reuse'),
                               PeerCacheTests('test_default_cache_is_workspace_shared_without_cwd_authority')])
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
