#!/usr/bin/env python3
"""Run the production Toasty/DatabaseOwner TLS contract against disposable PG17.

Requires a locally available official postgres:17 image, Docker, OpenSSL and
cargo-nextest. No global trust installation, existing database or Compose change.
Certificates exist only in a private temporary directory/container. No passwords
are needed: this loopback-only fixture deliberately permits trust authentication
and plaintext, so bad verify-full certificates cannot pass by TLS downgrade.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid
from urllib.parse import urlencode


def run(arguments, **kwargs):
    return subprocess.run(arguments, check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, timeout=60, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='postgres:17', help='Existing local official PG17 image')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    token = uuid.uuid4().hex
    name = 'rcoder-storage-tls-' + token[:12]
    label = 'rcoder.fixture.owner'
    container = None
    previous_umask = os.umask(0o077)
    try:
        run(['docker', 'image', 'inspect', args.image])
        with tempfile.TemporaryDirectory(prefix='rcoder-storage-tls-') as directory:
            fixture = Path(directory)
            for ca in ('trusted', 'untrusted'):
                run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                     '-keyout', str(fixture / (ca + '.key')), '-out', str(fixture / (ca + '.crt')),
                     '-subj', '/CN=RCoder isolated ' + ca + ' test CA'])
            run(['openssl', 'req', '-new', '-newkey', 'rsa:2048', '-nodes',
                 '-keyout', str(fixture / 'server.key'), '-out', str(fixture / 'server.csr'),
                 '-subj', '/CN=localhost'])
            (fixture / 'extensions.cnf').write_text(
                'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n'
                'keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
            run(['openssl', 'x509', '-req', '-days', '1', '-in', str(fixture / 'server.csr'),
                 '-CA', str(fixture / 'trusted.crt'), '-CAkey', str(fixture / 'trusted.key'),
                 '-CAcreateserial', '-out', str(fixture / 'server.crt'),
                 '-extfile', str(fixture / 'extensions.cnf')])
            for path in fixture.iterdir():
                path.chmod(0o600)
            container = run([
                'docker', 'create', '--name', name, '--label', label + '=' + token,
                '--publish', '127.0.0.1::5432', '-e', 'POSTGRES_HOST_AUTH_METHOD=trust',
                '-e', 'POSTGRES_DB=tlsfixture', '--entrypoint', 'sh', args.image, '-c',
                'chown postgres:postgres /tmp/server.key /tmp/server.crt && '
                'chmod 600 /tmp/server.key /tmp/server.crt && '
                'exec docker-entrypoint.sh postgres -c ssl=on '
                '-c ssl_cert_file=/tmp/server.crt -c ssl_key_file=/tmp/server.key',
            ]).stdout.strip()
            for filename in ('server.key', 'server.crt'):
                run(['docker', 'cp', str(fixture / filename), container + ':/tmp/' + filename])
            run(['docker', 'start', container])
            for _ in range(150):
                ready = subprocess.run(['docker', 'exec', container, 'pg_isready', '-U', 'postgres',
                                        '-d', 'tlsfixture'], capture_output=True, timeout=10)
                if ready.returncode == 0:
                    # Entrypoint briefly runs a socket-only bootstrap instance.
                    state = json.loads(run(['docker', 'inspect', container]).stdout)[0]
                    port = state['NetworkSettings']['Ports']['5432/tcp'][0]['HostPort']
                    probe = subprocess.run(['docker', 'exec', container, 'psql', '-h', '127.0.0.1',
                                            '-U', 'postgres', '-d', 'tlsfixture', '-Atc', 'SELECT 1'],
                                           capture_output=True, timeout=10)
                    if probe.returncode == 0:
                        break
                time.sleep(.2)
            else:
                raise RuntimeError('isolated PostgreSQL TLS fixture failed readiness')
            def dsn(host, ca, mode='verify-full'):
                query = {'sslmode': mode}
                if ca:
                    query['sslrootcert'] = str(fixture / (ca + '.crt'))
                return f'postgresql://postgres@{host}:{port}/tlsfixture?' + urlencode(query)
            environment = os.environ.copy()
            environment.update({
                'RCODER_TLS_GOOD_DSN': dsn('localhost', 'trusted'),
                'RCODER_TLS_BAD_CA_DSN': dsn('localhost', 'untrusted'),
                # Same reachable endpoint, absent IP SAN: only hostname differs.
                'RCODER_TLS_BAD_HOST_DSN': dsn('127.0.0.1', 'trusted'),
                'RCODER_TLS_PLAIN_DSN': dsn('127.0.0.1', None, 'disable'),
            })
            result = subprocess.run([
                'cargo', 'nextest', 'run', '-p', 'rcoder-storage', '--features', 'pg',
                '--no-fail-fast', '--run-ignored', 'all', '--test-threads', '1',
                '-E', 'test(db::pg_tls_tests::postgres_verify_full_uses_tls_and_rejects_wrong_ca_and_hostname)',
            ], cwd=root, env=environment)
            return result.returncode
    finally:
        if container:
            state = json.loads(run(['docker', 'inspect', container]).stdout)[0]
            if state['Config']['Labels'].get(label) != token or state['Name'] != '/' + name:
                raise RuntimeError('fixture ownership differs; refusing container cleanup')
            run(['docker', 'rm', '-f', '-v', container])
        os.umask(previous_umask)


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (subprocess.SubprocessError, OSError, RuntimeError, ValueError, KeyError):
        # Do not echo command output, private file contents or environment values.
        raise SystemExit('Isolated PG TLS contract failed; fixture or test did not complete') from None
