"""Local Docker admission for destructive, uniquely owned protocol fixtures."""
import ipaddress
import os
from pathlib import PurePosixPath
import subprocess
from urllib.parse import urlsplit


def require_local_docker_endpoint():
    # DOCKER_CONTEXT takes precedence over DOCKER_HOST in the Docker CLI.
    # Inspect the selected context without changing the user's global context.
    context = os.environ.get('DOCKER_CONTEXT', '').strip()
    endpoint = os.environ.get('DOCKER_HOST', '').strip()
    if context or not endpoint:
        command = ['docker', 'context', 'inspect']
        if context:
            command.append(context)
        command.extend(['--format', '{{(index .Endpoints "docker").Host}}'])
        try:
            inspected = subprocess.run(command, capture_output=True, text=True,
                                       check=False, timeout=15)
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError('Cannot inspect the selected Docker endpoint; no fallback is permitted') from error
        if inspected.returncode != 0:
            raise RuntimeError('Cannot inspect the selected Docker endpoint; no fallback is permitted')
        endpoint = inspected.stdout.strip()
    try:
        parsed = urlsplit(endpoint)
        clean = not (parsed.username or parsed.password or parsed.query or parsed.fragment)
        if parsed.scheme == 'unix':
            allowed = clean and not parsed.netloc and PurePosixPath(parsed.path).is_absolute()
        elif parsed.scheme == 'tcp':
            host = parsed.hostname or ''
            try:
                loopback = ipaddress.ip_address(host).is_loopback
            except ValueError:
                loopback = host.lower() == 'localhost'
            allowed = clean and loopback and parsed.path in ('', '/') and parsed.port is not None
        else:
            allowed = False
    except ValueError:
        allowed = False
    if not allowed:
        # Do not echo an endpoint URL which could contain credentials.
        raise RuntimeError('Isolated fixtures require a local Unix socket or loopback TCP Docker endpoint; remote or ambiguous endpoints are refused')
    return endpoint
