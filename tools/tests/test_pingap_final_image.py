"""Read actual image recipes; not a real image/deployment acceptance."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class OfficialImageTests(unittest.TestCase):
    def test_runtime_final_replaces_same_version_base_with_checked_official_asset(self):
        text = (ROOT / 'docker/app-runtime-base/Dockerfile.runtime').read_text()
        self.assertIn('install --download', text)
        self.assertIn('--arch ${TARGETARCH}', text)
        copy = text.rindex('/build/pingap-official/pingap /usr/local/bin/pingap')
        verify = text.rindex('RUN python3 /usr/local/share/rcoder/verify-pingap.py')
        self.assertLess(copy, verify)
        self.assertIn('/usr/local/share/rcoder/pingap.official.json', text[copy:])
        self.assertNotIn('pingap-applied', text)

    def test_agent_final_installs_frozen_official_archive_with_sha_gate(self):
        text = (ROOT / 'docker/rcoder-agent-runner/Dockerfile').read_text()
        self.assertIn('COPY pingap-official/', text)
        self.assertIn('pingap_official.py --version', text)
        self.assertIn('--archive "$TARBALL" --arch ${TARGETARCH}', text)
        self.assertNotIn('pingap-applied', text)

    def test_remote_hot_images_replace_proxy_and_app_cli_from_same_pin(self):
        text = (ROOT / 'docker/remote-k8s/Dockerfile').read_text()
        self.assertIn('pingap_official.py --repo-root /src install --download', text)
        for target in ('computer', 'runtime'):
            with self.subTest(target=target):
                stage = text.split(' AS ' + target + '\n', 1)[1].split('\nFROM ', 1)[0]
                self.assertIn('/out/app-cli', stage)
                self.assertIn('--from=pingap-official-build /out/pingap /usr/local/bin/pingap', stage)
                self.assertIn('--from=pingap-official-build /out/pingap.official.json', stage)
                self.assertIn('RUN python3 /usr/local/share/rcoder/verify-pingap.py', stage)
                self.assertNotIn('pingap-applied', stage)


if __name__ == '__main__':
    unittest.main()
