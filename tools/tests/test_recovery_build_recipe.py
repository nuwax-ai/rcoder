"""真实 recovery Make recipe 的产物身份协议；仅记录式 Cargo，不运行编译或 Docker。"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[2]


class RecoveryBuildRecipe(unittest.TestCase):
    def run_recipe(self, *, fail='', omit='', prior=False):
        fixtures = ROOT / '.cache' / 'recovery-recipe-contracts'
        fixtures.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='external target ', dir=fixtures) as temporary:
            root = Path(temporary)
            external = root / 'external cargo target'
            for package in ('app-cli', 'file-server-proxy'):
                manifest = root / 'crates' / package / 'Cargo.toml'
                manifest.parent.mkdir(parents=True)
                manifest.write_text(f'[package]\nname="{package}"\nversion="0.0.1"\nedition="2024"\n')
            (root / 'Cargo.toml').write_text('[workspace]\nmembers=["crates/file-server-proxy"]\nexclude=["crates/app-cli"]\n')
            # Future recipe helpers still run their actual source. Only Cargo is a stand-in.
            (root / 'tools').symlink_to(ROOT / 'tools', target_is_directory=True)
            (root / 'Makefile').write_text(f'include {ROOT}/make/test.mk\n')
            old_app = root / 'crates/app-cli/target/x86_64-unknown-linux-gnu/release/app-cli'
            old_proxy = root / 'target/x86_64-unknown-linux-gnu/release/file-server-proxy'
            for path in (old_app, old_proxy):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('historical-package:' + path.name)
                path.chmod(0o755)
            stub_bin = root / 'stub-bin'
            stub_bin.mkdir()
            stub = stub_bin / 'cargo'
            stub.write_text(textwrap.dedent('''\
                #!/usr/bin/env python3
                import json, os, pathlib, sys
                args=sys.argv[1:]
                package='app-cli' if '--manifest-path' in args else 'file-server-proxy'
                target=args[args.index('--target')+1].split('.')[0]
                root=pathlib.Path.cwd()
                with pathlib.Path(os.environ['COMMAND_LOG']).open('a') as log:
                    log.write(json.dumps({'args':args,'package':package,'target_dir':os.environ['CARGO_TARGET_DIR']})+'\\n')
                executable=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/target/'release'/package
                executable.parent.mkdir(parents=True,exist_ok=True)
                executable.write_text('this-build:'+package)
                executable.chmod(0o755)
                manifest=root/'crates'/package/'Cargo.toml'
                def event(name,path,manifest_path):
                    return {'reason':'compiler-artifact','package_id':f'path+file://{manifest_path.parent}#{name}@0.0.1',
                        'manifest_path':str(manifest_path),
                        'target':{'kind':['bin'],'crate_types':['bin'],'name':name,'src_path':str(manifest_path.parent/'src/main.rs')},
                        'profile':{'opt_level':'3','debuginfo':0,'debug_assertions':False,'overflow_checks':False,'test':False},
                        'features':[], 'filenames':[str(path)], 'executable':str(path), 'fresh':False}
                # Same target name from another package cannot authorize this binary.
                dependency=root/'dependency-target'/target/'release'/package
                dependency.parent.mkdir(parents=True,exist_ok=True)
                dependency.write_text('unrelated-dependency:'+package)
                print(json.dumps(event(package,dependency,root/'dependency/Cargo.toml')))
                if os.environ.get('OMIT_PACKAGE')!=package:
                    print(json.dumps(event(package,executable,manifest)))
                failed=os.environ.get('FAIL_PACKAGE')==package
                print(json.dumps({'reason':'build-finished','success':not failed}))
                sys.exit(42 if failed else 0)
                '''))
            stub.chmod(0o755)
            if prior:
                published = root / 'tests-e2e/reports/_bin'
                published.mkdir(parents=True)
                for name in ('app-cli-linux', 'file-server-proxy-linux'):
                    (published / name).write_text('valid-previous:' + name)
            log = root / 'calls.jsonl'
            env = dict(os.environ, PATH=f'{stub_bin}:{os.environ["PATH"]}',
                       CARGO_TARGET_DIR=str(external), COMMAND_LOG=str(log),
                       FAIL_PACKAGE=fail, OMIT_PACKAGE=omit)
            result = subprocess.run(['make', 'test-e2e-app-cli-recovery-build',
                                     'APP_CLI_RECOVERY_ARCH=x86_64'], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=20)
            output = root / 'tests-e2e/reports/_bin'
            copied = {name: (output / name).read_text() if (output / name).is_file() else None
                      for name in ('app-cli-linux', 'file-server-proxy-linux')}
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            originals = (old_app.read_text(), old_proxy.read_text())
            return result, copied, calls, originals

    def test_external_target_copies_this_build_not_historical_package(self):
        result, copied, calls, originals = self.run_recipe()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(copied, {'app-cli-linux': 'this-build:app-cli',
                                  'file-server-proxy-linux': 'this-build:file-server-proxy'})
        self.assertEqual([call['package'] for call in calls], ['app-cli', 'file-server-proxy'])
        self.assertTrue(all('--message-format=json' in call['args'] for call in calls))
        self.assertEqual(originals, ('historical-package:app-cli', 'historical-package:file-server-proxy'))

    def test_missing_current_artifact_cannot_copy_old_or_unrelated_package(self):
        result, copied, calls, _ = self.run_recipe(omit='file-server-proxy')
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIsNone(copied['file-server-proxy-linux'])
        self.assertEqual([call['package'] for call in calls], ['app-cli', 'file-server-proxy'])
        self.assertNotIn('构建完成', result.stdout)

    def test_cargo_failure_never_publishes_even_if_it_emitted_an_artifact(self):
        result, copied, calls, _ = self.run_recipe(fail='app-cli')
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(copied, {'app-cli-linux': None, 'file-server-proxy-linux': None})
        self.assertEqual([call['package'] for call in calls], ['app-cli'])
        self.assertNotIn('构建完成', result.stdout)

    def test_failed_build_preserves_existing_valid_publication(self):
        result, copied, calls, _ = self.run_recipe(fail='app-cli', prior=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(copied, {'app-cli-linux': 'valid-previous:app-cli-linux',
                                  'file-server-proxy-linux': 'valid-previous:file-server-proxy-linux'})
        self.assertEqual([call['package'] for call in calls], ['app-cli'])
        self.assertNotIn('构建完成', result.stdout)


if __name__ == '__main__':
    unittest.main()
