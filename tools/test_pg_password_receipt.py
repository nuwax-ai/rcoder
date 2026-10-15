"""Explicit real-PG receipt probe on the user's personal cluster, with isolated DB/roles.
Not part of ordinary unittest discovery; requires --run and SSH/kubectl access.
"""
import argparse
import concurrent.futures
from pathlib import Path
import subprocess
import shlex
import uuid

ROOT = Path(__file__).resolve().parents[1]
SQL = (ROOT / 'crates/shared_types/src/pg_password_receipt.sql').read_text()
CANCEL = (ROOT / 'crates/shared_types/src/pg_password_cancel.sql').read_text()
SCHEMA = (ROOT / 'crates/shared_types/src/pg_password_receipt_schema.sql').read_text()


def quote(value):
    return "E'" + value.replace('\\', '\\\\').replace("'", "''") + "'"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--run', action='store_true', required=True)
    parser.add_argument('--host', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--pod', required=True)
    args = parser.parse_args()
    # These identifiers are locally generated; no existing application DB/role is touched.
    name = 'rcoder_receipt_' + uuid.uuid4().hex[:12]
    role = name
    remote = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', args.host,
              'kubectl', '-n', args.namespace, 'exec', '-i', args.pod, '-c', 'postgres', '--',
              'psql', '-X', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-qAt']

    def run(sql, database=name, success=True):
        result = subprocess.run(remote + ['-d', database], input=sql, text=True,
                                capture_output=True, timeout=25)
        if (result.returncode == 0) != success:
            # Never print SQL or provider diagnostics containing private input.
            raise AssertionError('Unexpected PostgreSQL outcome: exit ' + str(result.returncode) + ' / ' + '\n'.join(line for line in result.stderr.splitlines() if 'ERROR:' in line))
        return result.stdout.strip()

    def operation(operation_id, fingerprint, password, create=False, username=role, pause=False):
        values = dict(app_id='receiptapp', lifecycle_id='receiptlife', scope='prod',
                      operation_id=operation_id, fingerprint=fingerprint,
                      username=username, private_password=password,
                      create_role='true' if create else 'false')
        settings = ''.join('SET LOCAL rcoder.' + key + ' = ' + quote(value) + ';\n'
                           for key, value in values.items())
        return (SCHEMA + '\nBEGIN ISOLATION LEVEL READ COMMITTED;\n'
                "SET LOCAL statement_timeout='5s'; SET LOCAL idle_in_transaction_session_timeout='5s';\n"
                "SET LOCAL log_statement='none'; SET LOCAL log_min_error_statement='panic';\n"
                + settings + SQL + ('\nSELECT pg_sleep(1);' if pause else '') + '\nCOMMIT;')

    def authenticate(password, expected):
        command = ['kubectl', '-n', args.namespace, 'exec', args.pod, '-c', 'postgres', '--',
                   'env', 'PGPASSWORD=' + password, 'psql', '-X', '-w', '-h', '127.0.0.1',
                   '-U', role, '-d', name, '-Atc', 'SELECT 1']
        result = subprocess.run(['ssh', '-o', 'BatchMode=yes', args.host, shlex.join(command)],
                                capture_output=True, text=True, timeout=15)
        assert (result.returncode == 0) == expected, 'TCP password authentication mismatch'

    created = False
    try:
        run('CREATE DATABASE ' + name + ';', 'postgres')
        created = True
        version = run('SHOW server_version;')
        run(operation('first', 'a' * 64, "fixture'\\$rcoder_password_operation$", create=True))
        authenticate("fixture'\\$rcoder_password_operation$", True)
        authenticate('definitely_wrong_fixture', False)
        digest_before = run("SELECT md5(rolpassword) FROM pg_authid WHERE rolname=" + quote(role))
        run(operation('second', 'b' * 64, 'fixture_second'))
        digest_after = run("SELECT md5(rolpassword) FROM pg_authid WHERE rolname=" + quote(role))
        assert digest_before != digest_after, 'second operation must change password'
        run(operation('first', 'a' * 64, "fixture'\\$rcoder_password_operation$"))
        assert run("SELECT md5(rolpassword) FROM pg_authid WHERE rolname=" + quote(role)) == digest_after, 'late replay overwrote newer password'
        run(operation('first', 'c' * 64, 'different_input'), success=False)
        assert run("SELECT md5(rolpassword) FROM pg_authid WHERE rolname=" + quote(role)) == digest_after
        run(operation('failed', 'd' * 64, 'fixture_failed', username=name + '_missing'), success=False)
        assert run("SELECT count(*) FROM rcoder_management.password_receipts WHERE operation_id='failed'") == '0', 'failed ALTER left a committed receipt'
        # Closing a session without COMMIT must roll back both password and receipt.
        run(operation('disconnected', 'f' * 64, 'fixture_uncommitted').removesuffix('COMMIT;'))
        assert run("SELECT count(*) FROM rcoder_management.password_receipts WHERE operation_id='disconnected'") == '0'
        assert run("SELECT md5(rolpassword) FROM pg_authid WHERE rolname=" + quote(role)) == digest_after
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            calls = [pool.submit(run, operation('concurrent', 'e' * 64, 'fixture_concurrent', pause=True)) for _ in range(2)]
            for call in calls:
                call.result()
        assert run("SELECT count(*) FROM rcoder_management.password_receipts WHERE operation_id='concurrent'") == '1'
        # Cancellation first: a delayed writer must never change the role.
        cancel = operation('cancelled', '1' * 64, 'unused').replace(SQL, CANCEL)
        assert 'cancelled' in run(cancel).splitlines()
        run(operation('cancelled', '1' * 64, 'late_password'), success=False)
        authenticate('fixture_concurrent', True)
        # Commit first: cancellation reports committed; it cannot claim rollback.
        assert 'committed' in run(operation('concurrent', 'e' * 64, 'unused').replace(SQL, CANCEL)).splitlines()
        # Changed identity must return no receipt, never authorize release.
        changed = run(operation('cancelled', '2' * 64, 'unused').replace(SQL, CANCEL))
        assert 'cancelled' not in changed.splitlines() and 'committed' not in changed.splitlines()
        # Two independent sessions race cancellation against the same writer.
        writer_sql = operation('cancelrace', '3' * 64, 'fixture_race', pause=True)
        cancel_sql = operation('cancelrace', '3' * 64, 'unused').replace(SQL, CANCEL)
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            writer = pool.submit(run, writer_sql, success=True)
            canceller = pool.submit(run, cancel_sql)
            cancellation = canceller.result()
            try:
                writer.result()
                writer_committed = True
            except AssertionError as error:
                if 'Database password operation was cancelled' not in str(error):
                    raise
                writer_committed = False
        outcome = run("SELECT outcome FROM rcoder_management.password_receipts WHERE operation_id='cancelrace'")
        assert outcome == ('committed' if writer_committed else 'cancelled')
        assert outcome in cancellation.splitlines()
        authenticate('fixture_race' if writer_committed else 'fixture_concurrent', True)
        print('PASS: cancellation before/after write, changed identity, concurrent cancellation race; create, later change, late replay fence, changed-input rejection, transactional rollback, session rollback, TCP authentication, concurrent duplicate; PG ' + version)
    finally:
        if created:
            run('DROP DATABASE ' + name + ' WITH (FORCE);', 'postgres')
            run('DROP ROLE IF EXISTS ' + role + ';', 'postgres')
            print('Owned temporary database and role cleaned up')


if __name__ == '__main__':
    main()
