"""回归协议证据入口：禁止错资产、空配置和伪造健康观察被记为成功。"""
import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "verify_pingap_upgrade.py"
if not SOURCE.is_file():
    SOURCE = Path(__file__).resolve().with_name("verify_pingap_upgrade.py")
SPEC = importlib.util.spec_from_file_location("verify_pingap_upgrade", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class UpgradeEvidenceTests(unittest.TestCase):
    def test_official_arch_named_member_is_extracted_to_fixed_binary_path(self):
        # The real v0.15.0 archives contain ./pingap-linux-gnu-<arch>-full,
        # not a member named pingap. A bare-name fixture missed this failure.
        for architecture in ("x86", "aarch64"):
            with self.subTest(architecture=architecture), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                asset, binary = root / "asset.tar.gz", root / "pingap"
                name = f"pingap-linux-gnu-{architecture}-full"
                with tarfile.open(asset, "w:gz") as archive:
                    member = tarfile.TarInfo("./" + name)
                    member.size = 3
                    archive.addfile(member, io.BytesIO(b"bin"))
                digest = hashlib.sha256(asset.read_bytes()).hexdigest()
                MODULE.checked_binary(asset, digest, binary, name)
                self.assertEqual(binary.read_bytes(), b"bin")

    def test_missing_or_multiple_config_observations_are_not_success(self):
        expected = "🛡 Pingap 生效配置 (profile = prod, expected hash = A123):\n"
        body = MODULE.PREFIX_START + '[servers.app]\naddr="0.0.0.0:9080"\n' + MODULE.PREFIX_END
        config, digest = MODULE.compiled_config(expected + body)
        self.assertEqual(digest, "A123")
        self.assertIn('[servers.app]', config)
        for broken in [body, expected, expected + body + body]:
            with self.subTest(broken=broken), self.assertRaises(ValueError):
                MODULE.compiled_config(broken)

    def test_wrong_sha_refuses_asset_before_binary_is_written(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            asset, binary = root / "asset.tar.gz", root / "pingap"
            with tarfile.open(asset, "w:gz") as archive:
                member = tarfile.TarInfo("../../pingap")
                payload = b"controlled asset bytes"
                member.size = len(payload)
                archive.addfile(member, io.BytesIO(payload))
            with self.assertRaises(ValueError):
                MODULE.checked_binary(asset, "0" * 64, binary)
            self.assertFalse(binary.exists())
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            MODULE.checked_binary(asset, digest, binary)
            self.assertEqual(binary.read_bytes(), payload)
            self.assertTrue(binary.stat().st_mode & 0o100)
            self.assertEqual(sorted(path.name for path in root.iterdir()), ["asset.tar.gz", "pingap"])

    def test_ambiguous_binary_archive_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            asset = root / "asset.tar.gz"
            with tarfile.open(asset, "w:gz") as archive:
                for name in ["one/pingap", "two/pingap"]:
                    member = tarfile.TarInfo(name)
                    member.size = 3
                    archive.addfile(member, io.BytesIO(b"bin"))
            with self.assertRaises(ValueError):
                MODULE.checked_binary(asset, hashlib.sha256(asset.read_bytes()).hexdigest(),
                                      root / "pingap")

    def test_partial_or_empty_health_snapshot_never_means_ready(self):
        for snapshot in [{}, {"upstream_healthy_status": {}},
                         {"upstream_healthy_status": {"api": {"healthy": 1, "total": 1}}},
                         {"upstream_healthy_status": {"api": {"healthy": 1, "total": 2},
                                                       "root": {"healthy": 1, "total": 1}}}]:
            with self.subTest(snapshot=snapshot):
                self.assertIsNone(MODULE.health_snapshot(lambda: snapshot, 2, 1))
        snapshot = {"upstream_healthy_status": {"api": {"healthy": 1, "total": 1},
                                                "root": {"healthy": 1, "total": 1}}}
        self.assertEqual(MODULE.health_snapshot(lambda: snapshot, 2, 1), snapshot)


if __name__ == "__main__":
    unittest.main()
