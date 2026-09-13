"""Configuration and checked subprocess boundary; never execute dotenv as shell."""
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
VERSION = '0.18.1'
LABEL = 'rcoder.dev/environment'
EXCLUDES = ['.git', '.env*', '*.env.local', '*.pem', '*.key', 'id_rsa*', 'id_ed25519*',
            '.ssh', '.aws', '.kube', '.codex', 'target', 'target-*', 'node_modules',
            '__pycache__', '*.log', '/logs', 'tests-e2e/reports', '.remote-k8s']


def run(args, *, data=None, timeout=120, env=None, log=None, guard=None):
    args = [str(x) for x in args]
    if log is None:
        result = subprocess.run(args, input=data, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, timeout=timeout, env=env)
        output, errors, code = result.stdout, result.stderr, result.returncode
    else:
        log.parent.mkdir(parents=True, exist_ok=True)
        print('Log: ' + str(log), flush=True)
        with log.open('w') as stream:
            log.chmod(0o600)
            process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=stream, stderr=subprocess.STDOUT,
                                       text=True, env=env, start_new_session=True)
            try:
                if guard is None:
                    process.communicate(data, timeout=timeout)
                else:
                    deadline = time.monotonic() + timeout
                    pending = data
                    while True:
                        guard()
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise subprocess.TimeoutExpired(args, timeout)
                        try:
                            process.communicate(pending, timeout=min(1, remaining))
                            break
                        except subprocess.TimeoutExpired:
                            pending = None
            except BaseException:
                process.terminate()
                try:
                    process.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                raise
        output = log.read_text()
        errors, code = output, process.returncode
    sensitive = [v for k, v in (env or os.environ).items() if re.search(r'API_KEY|PASSWORD|TOKEN|SECRET', k) and len(v) > 5]
    for value in sorted(sensitive, key=len, reverse=True):
        output = output.replace(value, '<redacted>')
        errors = errors.replace(value, '<redacted>')
    if log is not None:
        log.write_text(output)
    if code:
        # Command arguments/stdin may carry credentials. Do not include them in errors.
        raise RuntimeError(f'{Path(args[0]).name} exited {code}: {errors[-1800:]}')
    return output


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.chmod(0o600)
    temporary.replace(path)


class Config:
    def __init__(self, file=None):
        self.values = {}
        file = Path(file or os.environ.get('REMOTE_K8S_ENV_FILE', ROOT / '.env.local'))
        if file.exists():
            for line in file.read_text().splitlines():
                line = line.strip()
                if not line or line.startswith('#'):
                    continue
                key, sep, value = line.removeprefix('export ').partition('=')
                if not sep or not re.fullmatch(r'[A-Z][A-Z0-9_]*', key.strip()):
                    raise ValueError('Invalid dotenv assignment')
                parts = shlex.split(value, comments=True)
                if len(parts) > 1:
                    raise ValueError(f'Quote spaces in {key}')
                self.values[key.strip()] = parts[0] if parts else ''
        self.values.update(os.environ)
        self.host = self.get('SSH', required=True)
        if not re.fullmatch(r'[a-zA-Z0-9_.@-]+', self.host) or self.host.startswith('-'):
            raise ValueError('SSH must be an SSH alias or user@host; configure options in ~/.ssh/config')
        self.remote = self.get('DIR', required=True).rstrip('/')
        if not re.fullmatch(r'/[a-zA-Z0-9_./-]+', self.remote) or '..' in Path(self.remote).parts or len(Path(self.remote).parts) < 4:
            raise ValueError('DIR must be a dedicated absolute path, at least two levels below /')
        self.context = self.get('CONTEXT', required=True)
        if not re.fullmatch(r'[a-zA-Z0-9_@./:-]+', self.context):
            raise ValueError('CONTEXT contains unsupported characters')
        self.ns = self.get('NAMESPACE', 'rcoder-e2e-soddy')
        if not re.fullmatch(r'rcoder-e2e-[a-z0-9][a-z0-9-]{0,35}', self.ns):
            raise ValueError('Namespace must be a dedicated rcoder-e2e-* name')
        self.id = digest([self.host, self.context, self.ns])[:16]
        self.session = 'rcoder-' + digest([str(ROOT), self.host, self.remote])[:16]
        self.state = ROOT / '.remote-k8s' / self.id
        self.mutagen = self.get('MUTAGEN', 'mutagen')
        self.timeout = int(self.get('TIMEOUT', '180'))
        self.nodeport = int(self.get('NODEPORT', '31290'))
        if not 30000 <= self.nodeport <= 32767:
            raise ValueError('NODEPORT outside Kubernetes default range')
        self.url = self.get('URL', '').rstrip('/')
        for key in ['JOBS', 'CPUS', 'BUILD_TIMEOUT', 'TIMEOUT']:
            if int(self.get(key, '4')) <= 0:
                raise ValueError(key + ' must be positive')
        if self.get('REGISTRY_AUTH', 'none') not in ['none', 'docker']:
            raise ValueError('REGISTRY_AUTH must be none or docker')
        if not re.fullmatch(r'https?://[a-zA-Z0-9.:-]+', self.get('APT_MIRROR', 'http://deb.debian.org')):
            raise ValueError('APT_MIRROR must be an HTTP(S) base URL')
        if not re.fullmatch(r'[a-z0-9][a-z0-9_.-]*', self.get('IMAGE_PREFIX', self.ns + '-')):
            raise ValueError('Invalid IMAGE_PREFIX')

    def get(self, name, default='', required=False):
        value = self.values.get('REMOTE_K8S_' + name, default)
        if required and not value:
            raise ValueError('Missing REMOTE_K8S_' + name)
        return value

    def ssh(self, args, data=None, timeout=None, log=None):
        return run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-o', 'ServerAliveInterval=10',
                    '-o', 'ServerAliveCountMax=2', self.host, shlex.join([str(a) for a in args])],
                   data=data, timeout=timeout or self.timeout, log=log)

    def kube(self, *args, data=None, timeout=None):
        return self.ssh(['kubectl', '--context', self.context, '--namespace', self.ns, *args], data, timeout=timeout)

    def obj(self, *args):
        return json.loads(self.kube(*args, '-o', 'json'))

    def mut(self, *args):
        return run([self.mutagen, *args], timeout=self.timeout)

    def ignores(self):
        lines = (ROOT / '.gitignore').read_text().splitlines()
        return list(dict.fromkeys(EXCLUDES + [x.strip() for x in lines if x.strip() and not x.startswith('#')]))
