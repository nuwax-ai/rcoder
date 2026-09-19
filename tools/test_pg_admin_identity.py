"""Shell protocol tests only; these do not replace real PostgreSQL image tests."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPERS = [ROOT / 'docker' / image / 'pg-admin-identity.sh'
           for image in ('app-runtime-base', 'rcoder-agent-runner')]


class AdministratorIdentityTests(unittest.TestCase):
    def run_shell(self, helper, directory, script, **values):
        env = dict(os.environ, PGDATA=str(directory), POSTGRES_USER='initialadmin')
        env.pop('APP_PG_ADMIN_USER', None)
        env.update(values)
        return subprocess.run(['sh', '-c', '. "$1"; ' + script, 'test', str(helper)],
                              env=env, capture_output=True, text=True)

    def test_reload_keeps_original_administrator(self):
        for helper in HELPERS:
            with self.subTest(helper=helper), tempfile.TemporaryDirectory() as directory:
                first = self.run_shell(helper, directory,
                                       'pg_admin_identity_load && pg_admin_identity_record')
                self.assertEqual(first.returncode, 0, first.stderr)
                marker = Path(directory) / '.rcoder-admin-user'
                self.assertEqual(marker.read_text(), 'initialadmin\n')
                self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
                reload = self.run_shell(helper, directory,
                                        'pg_admin_identity_load && printf "%s" "$PG_ADMIN_USER"',
                                        POSTGRES_USER='newbusiness')
                self.assertEqual(reload.returncode, 0, reload.stderr)
                self.assertEqual(reload.stdout, 'initialadmin')
                self.assertEqual(list(Path(directory).iterdir()), [marker])

    def test_conflict_and_corruption_are_not_overwritten(self):
        for helper in HELPERS:
            with self.subTest(helper=helper), tempfile.TemporaryDirectory() as directory:
                marker = Path(directory) / '.rcoder-admin-user'
                marker.write_text('original\n')
                conflict = self.run_shell(helper, directory, 'pg_admin_identity_load',
                                          APP_PG_ADMIN_USER='other')
                self.assertNotEqual(conflict.returncode, 0)
                self.assertEqual(marker.read_text(), 'original\n')
                for invalid in ('', 'bad-name\n', 'first\nsecond\n', 'a' * 64):
                    marker.write_text(invalid)
                    self.assertNotEqual(self.run_shell(helper, directory,
                                                        'pg_admin_identity_load').returncode, 0)
                    self.assertEqual(marker.read_text(), invalid)

    def test_late_record_cannot_replace_winner(self):
        for helper in HELPERS:
            with self.subTest(helper=helper), tempfile.TemporaryDirectory() as directory:
                result = self.run_shell(helper, directory,
                    'pg_admin_identity_load && pg_admin_identity_record && '
                    'PG_ADMIN_USER=lateadmin; pg_admin_identity_record')
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((Path(directory) / '.rcoder-admin-user').read_text(), 'initialadmin\n')


class DatabaseBootstrapTests(unittest.TestCase):
    def bootstrap(self, helper, mode):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            psql = bin_dir / 'psql'
            psql.write_text("""#!/bin/sh
[ -z "${PGSERVICE+x}" ] && [ -z "${PGHOSTADDR+x}" ] || exit 71
case "$*" in
  *pg_roles*) printf '1\\n'; exit 0 ;;
esac
cat >/dev/null
if [ -f "$PGDATA/database-exists" ]; then printf '1\\n'; fi
""")
            createdb = bin_dir / 'createdb'
            createdb.write_text("""#!/bin/sh
[ -z "${PGSERVICE+x}" ] && [ -z "${PGHOSTADDR+x}" ] || exit 71
touch "$PGDATA/create-attempted"
case "$BOOTSTRAP_TEST_MODE" in
  failed) exit 1 ;;
  raced) touch "$PGDATA/database-exists"; exit 1 ;;
  success) touch "$PGDATA/database-exists" ;;
esac
""")
            for command in (psql, createdb):
                command.chmod(0o700)
            if mode == 'existing':
                (root / 'database-exists').touch()
            env = dict(os.environ, PGDATA=directory, PG_BIN=str(bin_dir),
                       POSTGRES_USER='initialadmin', POSTGRES_DB="business'quoted",
                       BOOTSTRAP_TEST_MODE=mode, PGSERVICE='unrelated-service', PGHOSTADDR='192.0.2.1')
            env.pop('APP_PG_ADMIN_USER', None)
            result = subprocess.run(['sh', '-c',
                '. "$1"; pg_admin_identity_load && pg_bootstrap_database',
                'test', str(helper)], env=env, capture_output=True, text=True, timeout=10)
            return result, (root / 'create-attempted').exists()

    def test_missing_database_failure_is_not_success(self):
        for helper in HELPERS:
            with self.subTest(helper=helper):
                result, attempted = self.bootstrap(helper, 'failed')
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(attempted)

    def test_existing_database_does_not_run_createdb(self):
        for helper in HELPERS:
            with self.subTest(helper=helper):
                result, attempted = self.bootstrap(helper, 'existing')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(attempted)

    def test_creation_and_confirmed_race_both_succeed(self):
        for helper in HELPERS:
            for mode in ('success', 'raced'):
                with self.subTest(helper=helper, mode=mode):
                    result, attempted = self.bootstrap(helper, mode)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue(attempted)


if __name__ == '__main__':
    unittest.main()
