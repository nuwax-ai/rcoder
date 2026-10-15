"""Small, redacted cleanup diagnostics; never retain command argv or credentials."""
import json
import os
import subprocess
import urllib.error


def secrets_from(container=None):
    pairs = list(os.environ.items())
    for value in ((container or {}).get('Config', {}).get('Env') or []):
        key, _, secret = value.partition('=')
        pairs.append((key, secret))
    return sorted({value for key, value in pairs if value and any(word in key.upper() for word in ('KEY', 'TOKEN', 'SECRET', 'PASSWORD'))}, key=len, reverse=True)


def redact(text, secrets):
    if isinstance(text, bytes):
        text = text.decode('utf-8', errors='replace')
    for secret in secrets:
        text = text.replace(secret, '[REDACTED]')
    return text


def transport_failure(error, secrets):
    result = {'category': 'transport', 'error_type': type(error).__name__}
    if isinstance(error, subprocess.CalledProcessError):
        result['exit_code'] = error.returncode
        result['stderr'] = redact(error.stderr or error.output or '', secrets)
    elif isinstance(error, (subprocess.TimeoutExpired, TimeoutError)):
        result['transport'] = 'timeout'
        if isinstance(error, subprocess.TimeoutExpired):
            result['timeout_seconds'] = error.timeout
            result['stderr'] = redact(error.stderr or '', secrets)
    elif isinstance(error, urllib.error.HTTPError):
        result['http_status'] = error.code
        result['detail'] = redact(str(error.reason), secrets)
    elif isinstance(error, urllib.error.URLError):
        result['detail'] = redact(str(error.reason), secrets)
    else:
        result['detail'] = redact(str(error), secrets)
    return result


class PurgeRejected(RuntimeError):
    def __init__(self, body, secrets):
        self.diagnostic = {'category': 'business', 'code': redact(str(body.get('code')), secrets),
                           'message': redact(str(body.get('message')), secrets)}
        super().__init__(json.dumps(self.diagnostic))
