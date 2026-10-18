"""Harness gates for wheel, frozen input, ZIP, counter and cleanup identity; no E2E claim."""
import base64
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
import zipfile

import python_dependency_cache_contract as cache


def source_files():
    return {
        "workspace.manifest.toml": b'schema_version=1\n[workspace]\nname="test"\n',
        "backend-python/project.manifest.toml": b'''schema_version=1
[project]
service_id="backend-python"
name="Python"
type="python"
[build]
command = ["sh", "scripts/build-standalone.sh"]
artifact="artifact.zip"
[run]
command=["python3","main.py"]
''',
        "backend-python/scripts/build-standalone.py": b"# frozen actual build input\n",
        "backend-python/scripts/build-standalone.sh": b"exec python3 scripts/build-standalone.py\n",
        "backend-python/scripts/artifact_pack.py": b"# frozen actual runtime artifact helper\n",
        "cli/package.json": b'{"version":"1.0.0"}',
    }


class CacheContractTests(unittest.TestCase):
    def test_valid_wheel_record_and_real_pip_install(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            wheel = root / cache.create_wheel(root)
            with zipfile.ZipFile(wheel) as archive:
                record = archive.read("cache_probe-1.0.dist-info/RECORD").decode()
                for line in record.splitlines():
                    name, expected, size = line.split(",")
                    if not expected:
                        continue
                    raw = archive.read(name)
                    self.assertEqual(expected, "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(raw).digest()).decode().rstrip("="))
                    self.assertEqual(int(size), len(raw))
            install = subprocess.run([sys.executable, "-m", "pip", "install", "--no-index", "--no-deps",
                                      "--disable-pip-version-check", "--target", str(root / "deps"), str(wheel)],
                                     capture_output=True, text=True, timeout=30)
            self.assertEqual(install.returncode, 0, install.stdout + install.stderr)
            probe = subprocess.run([sys.executable, "-c", "import sys;sys.path.insert(0,sys.argv[1]);import cache_probe;print(cache_probe.VERSION)",
                                    str(root / "deps")], check=True, capture_output=True, text=True, timeout=10)
            self.assertEqual(probe.stdout.strip(), "1.0")

    def test_fixture_scripts_are_unmodified_and_only_local_index_is_used(self):
        original = source_files()
        raw, hashes = cache.fixture_zip(original, "http://172.20.0.20:8080/simple/", "sentinel")
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            for name in ("backend-python/scripts/build-standalone.py", "backend-python/scripts/build-standalone.sh",
                         "backend-python/scripts/artifact_pack.py"):
                self.assertEqual(archive.read(name), original[name])
                self.assertEqual(hashes[name], cache.digest(original[name]))
            self.assertEqual(archive.read("backend-python/requirements.lock"), b"cache-probe==1.0\n")
            manifest = archive.read("backend-python/project.manifest.toml").decode()
            self.assertIn("PIP_CONFIG_FILE=/dev/null", manifest)
            self.assertIn("PIP_INDEX_URL=http://172.20.0.20:8080/simple/", manifest)

    def test_unknown_template_command_fails_without_silent_reimplementation(self):
        data = source_files()
        data["backend-python/project.manifest.toml"] = data["backend-python/project.manifest.toml"].replace(b'scripts/build-standalone.sh', b'scripts/new-build.sh')
        with self.assertRaisesRegex(ValueError, "uniquely replaceable"):
            cache.fixture_zip(data, "http://private:8080/simple/", "marker")

    def test_frozen_template_identity_contains_actual_build_hash(self):
        files = source_files()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name, raw in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(raw)
            with patch.object(cache.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "abcdef\n", "")):
                data, identity = cache.frozen_template(root)
            self.assertEqual(data, files)
            self.assertEqual(identity["commit"], "abcdef")
            self.assertEqual(identity["files_sha256"]["backend-python/scripts/build-standalone.py"], cache.digest(files["backend-python/scripts/build-standalone.py"]))
            self.assertEqual(identity["files_sha256"]["backend-python/scripts/artifact_pack.py"], cache.digest(files["backend-python/scripts/artifact_pack.py"]))

    def test_missing_actual_helper_fails_before_recording_frozen_identity(self):
        files = source_files()
        files.pop("backend-python/scripts/artifact_pack.py")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name, raw in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(raw)
            with patch.object(cache.subprocess, "run") as git:
                with self.assertRaises(FileNotFoundError):
                    cache.frozen_template(root)
                git.assert_not_called()

    def test_counter_filters_health_and_requires_real_index_or_wheel(self):
        self.assertEqual(cache.dependency_requests(["/__stats", "/favicon.ico", "/simple/cache-probe/", "/cache_probe.whl"]),
                         ["/simple/cache-probe/", "/cache_probe.whl"])
        self.assertEqual(cache.dependency_requests(["/__stats"]), [])

    def artifact_pair(self, root, leaked=False):
        child, package = root / "child.zip", root / "package.zip"
        entries = {"deps/cache_probe.py": b"VERSION='1.0'\n", "deps/cache_probe-1.0.dist-info/METADATA": b"Name: cache-probe\n"}
        if leaked:
            entries[".deps-stamp"] = b"private"
        for path, prefix in ((child, ""), (package, "backend-python/")):
            with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
                for name, raw in entries.items():
                    archive.writestr(prefix + name, raw)
        return child, package

    def test_zip_raw_payload_and_metadata_are_checked(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            child, package = self.artifact_pair(root)
            self.assertTrue(cache.inspect_artifacts(child, package)["raw_copy_verified"])
            with zipfile.ZipFile(package, "w", zipfile.ZIP_STORED) as archive:
                with zipfile.ZipFile(child) as original:
                    for name in original.namelist():
                        archive.writestr("backend-python/" + name, original.read(name))
            with self.assertRaisesRegex(ValueError, "changed child ZIP metadata"):
                cache.inspect_artifacts(child, package)

    def test_cache_files_are_rejected_in_both_artifacts(self):
        with tempfile.TemporaryDirectory() as temp:
            child, package = self.artifact_pair(Path(temp), leaked=True)
            with self.assertRaisesRegex(ValueError, "cache or lock leaked"):
                cache.inspect_artifacts(child, package)

    def test_recreation_rejects_volume_or_lifecycle_changes(self):
        before = {"id": "old", "image": "image", "name": "/rcoder-app-builder-app", "app_id": "app",
                  "lifecycle_id": "life", "mounts": [{"Source": "/owned/source"}]}
        after = dict(before, id="new")
        cache.prove_recreation(before, after)
        for field in ("image", "lifecycle_id", "mounts"):
            with self.assertRaises(ValueError):
                cache.prove_recreation(before, dict(after, **{field: "different"}))
        with self.assertRaises(ValueError):
            cache.prove_recreation(before, before)

    def test_foreign_builder_identity_never_authorizes_cleanup(self):
        row = {"Id": "id", "Image": "image", "Name": "/rcoder-app-builder-app", "Config": {"Labels": {
            "service-type": "user-app-builder", "rcoder.io/application-id": "other", "rcoder.io/lifecycle-id": "life"}},
            "Mounts": [{"Destination": "/home/user/app", "Type": "bind", "Source": "/owned/source"}]}
        with self.assertRaisesRegex(ValueError, "identity differs"):
            cache.compact_builder(row, "app")

    def test_cleanup_rejects_other_run_before_any_docker_operation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            (root / "ownership.json").write_text(json.dumps({"run_id": "foreign", "case_id": "case", "root": str(root)}))
            with patch.object(cache, "docker") as docker:
                with self.assertRaisesRegex(ValueError, "receipt differs"):
                    cache.cleanup(root, "run", "case")
                docker.assert_not_called()

    def test_unknown_create_with_empty_inventory_is_not_reported_as_cleaned(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            receipt = {"run_id": "run", "case_id": "case", "root": str(root),
                       "app_id": "pyc" + "a" * 19, "index_name": "rcoder-pip-index-" + "b" * 16,
                       "index_creation_state": "creating", "builder_creation_state": "creating"}
            (root / "ownership.json").write_text(json.dumps(receipt))
            with patch.object(cache, "docker", return_value=subprocess.CompletedProcess([], 0, "", "")):
                result = cache.cleanup(root, "run", "case")
            self.assertFalse(result["ok"])
            self.assertEqual(len(result["unsettled"]), 2)

    def test_captured_foreign_generation_is_not_removed_by_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            app = "pyc" + "a" * 19
            row = {"Id": "new", "Image": "image", "Name": "/rcoder-app-builder-" + app,
                   "Config": {"Labels": {"service-type": "user-app-builder", "rcoder.io/application-id": app,
                                          "rcoder.io/lifecycle-id": "different-life"}},
                   "Mounts": [{"Destination": "/home/user/" + app, "Type": "bind", "Source": "/owned/source"}]}
            receipt = {"run_id": "run", "case_id": "case", "root": str(root), "app_id": app,
                       "index_name": "rcoder-pip-index-" + "b" * 16,
                       "builder": {"id": "old", "image": "image", "lifecycle_id": "old-life", "mounts": []}}
            (root / "ownership.json").write_text(json.dumps(receipt))
            calls = []
            def docker(*args, **kwargs):
                calls.append(args)
                return subprocess.CompletedProcess([], 0, "new\n" if args[0] == "ps" else json.dumps([row]), "")
            with patch.object(cache, "docker", side_effect=docker):
                with self.assertRaisesRegex(ValueError, "captured exact physical identity"):
                    cache.cleanup(root, "run", "case")
            self.assertFalse(any(args[0] == "rm" for args in calls))


class StopRemovalTests(unittest.TestCase):
    def setUp(self):
        self.now = 100.0
        self.cid = "a" * 64
        self.before = {"id": self.cid, "lifecycle_id": "original-life"}
        self.operation = "original-stop-operation"
        self.path = "/computer/pod/operations/owned-app/" + self.operation
        self.contract = object.__new__(cache.Contract)
        self.contract.app = "owned-app"
        self.contract.api = Mock(return_value=(200, {"data": self.record()}))
        self.clock = patch.object(cache.time, "monotonic", side_effect=lambda: self.now)
        self.sleep = patch.object(cache.time, "sleep", side_effect=self.advance)
        self.clock.start()
        self.sleep.start()
        self.addCleanup(self.clock.stop)
        self.addCleanup(self.sleep.stop)

    def advance(self, seconds):
        self.now += seconds

    def record(self, **changes):
        return dict({"operation_id": self.operation, "app_id": "owned-app",
                     "lifecycle_id": "original-life", "scope": "Dev", "action": "stop",
                     "state": "succeeded", "stage": "completed"}, **changes)

    def removed(self, **changes):
        values = dict(returncode=1, stdout="[]\n", stderr="Error: No such object: " + self.cid + "\n")
        values.update(changes)
        return subprocess.CompletedProcess([], **values)

    def wait(self, seconds=240):
        return self.contract.wait_stopped_builder_removed(
            self.before, self.operation, self.path, self.now + seconds)

    def test_original_succeeded_stop_waits_for_exact_autoremove_without_another_write(self):
        results = [subprocess.CompletedProcess([], 0, json.dumps([{"Id": self.cid}]), ""), self.removed()]
        calls = []
        def inspect(*args, **kwargs):
            calls.append((args, kwargs, self.now))
            self.advance(0.027)
            return results.pop(0)
        with patch.object(cache, "docker", side_effect=inspect):
            stopped, absent = self.wait()
        self.assertTrue(absent)
        self.assertEqual(stopped, self.record())
        self.assertEqual(self.contract.api.call_count, 2)
        self.assertEqual([call.args for call in self.contract.api.call_args_list], [(self.path,), (self.path,)])
        self.assertEqual([args for args, _, _ in calls], [("inspect", self.cid), ("inspect", self.cid)])
        for _, kwargs, started in calls:
            self.assertFalse(kwargs["check"])
            self.assertLessEqual(kwargs["timeout"], min(180, 340 - started))
        self.assertLess(self.now, 340)

    def test_unknown_inspection_errors_and_changed_physical_identity_are_never_gone(self):
        for response in [
            self.removed(stderr="Cannot connect to the Docker daemon"),
            self.removed(stderr="Error: No such object: " + "b" * 64),
            self.removed(returncode=2),
            self.removed(returncode=0, stdout="[]"),
            self.removed(returncode=0, stdout=json.dumps([{"Id": "b" * 64}])),
            self.removed(returncode=0, stdout="invalid JSON"),
        ]:
            with self.subTest(response=response), patch.object(cache, "docker", return_value=response):
                with self.assertRaises((RuntimeError, ValueError)):
                    self.wait()

    def test_one_deadline_covers_success_receipt_and_still_present_cid(self):
        self.contract.api.return_value = (200, {"data": self.record()})
        with patch.object(cache, "docker", return_value=subprocess.CompletedProcess(
                [], 0, json.dumps([{"Id": self.cid}]), "")) as docker:
            with self.assertRaisesRegex(RuntimeError, "deadline exceeded"):
                self.wait(seconds=0.1)
        self.assertEqual(self.now, 100.1)
        self.assertEqual(docker.call_count, 1)
        self.assertLessEqual(docker.call_args.kwargs["timeout"], 0.1 + 1e-9)
        self.assertLessEqual(self.contract.api.call_args.kwargs["timeout"], 0.1 + 1e-9)

    def test_late_operation_or_absence_reply_cannot_restart_the_budget(self):
        def late_operation(*args, **kwargs):
            self.advance(0.2)
            return 200, {"data": self.record()}
        self.contract.api.side_effect = late_operation
        with patch.object(cache, "docker") as docker:
            with self.assertRaisesRegex(RuntimeError, "deadline exceeded"):
                self.wait(seconds=0.1)
            docker.assert_not_called()
        self.contract.api.side_effect = None
        def late_absence(*args, **kwargs):
            self.advance(0.2)
            return self.removed()
        with patch.object(cache, "docker", side_effect=late_absence):
            with self.assertRaisesRegex(RuntimeError, "deadline exceeded"):
                self.wait(seconds=0.1)

    def test_wrong_original_operation_and_failed_receipts_cannot_authorize_inspection(self):
        for change in [{"operation_id": "another-op"}, {"app_id": "foreign"},
                       {"lifecycle_id": "other-life"}, {"scope": "Production"},
                       {"action": "restart"}, {"state": "failed"},
                       {"state": "recovery_required"}, {"state": "superseded"},
                       {"stage": "stopping"}, {"state": "unknown"}]:
            self.contract.api.return_value = (200, {"data": self.record(**change)})
            with self.subTest(change=change), patch.object(cache, "docker") as docker:
                with self.assertRaises(RuntimeError):
                    self.wait()
                docker.assert_not_called()


if __name__ == "__main__":
    unittest.main()
