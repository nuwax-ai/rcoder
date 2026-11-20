//! Read the embedded file peer's effective process environment. Never inspect
//! a newly rotated Secret or the current platform recipe as a substitute.
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, ExecResult,
};

const READ_TOKEN: &str = r#"
import os, sys
peers = []
try:
    listeners = set()
    port = int(sys.argv[1])
    for table in ('/proc/net/tcp', '/proc/net/tcp6'):
        try:
            with open(table) as stream:
                for line in stream:
                    fields = line.split()
                    if len(fields) > 9 and fields[3] == '0A' and int(fields[1].rsplit(':', 1)[1], 16) == port:
                        listeners.add('socket:[' + fields[9] + ']')
        except FileNotFoundError:
            pass
    if not listeners:
        sys.exit(3)
    for name in os.listdir('/proc'):
        if not name.isdecimal():
            continue
        try:
            fds = os.listdir('/proc/' + name + '/fd')
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        owns_listener = False
        for fd in fds:
            try:
                owns_listener = owns_listener or os.readlink('/proc/' + name + '/fd/' + fd) in listeners
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
        if not owns_listener:
            continue
        try:
            exe = os.readlink('/proc/' + name + '/exe')
        except (FileNotFoundError, ProcessLookupError):
            continue
        if exe.removesuffix(' (deleted)').rsplit('/', 1)[-1] != 'agent_runner':
            sys.exit(6)
        try:
            with open('/proc/' + name + '/environ', 'rb') as stream:
                values = stream.read().split(b'\0')
        except (FileNotFoundError, ProcessLookupError):
            continue
        peers.append(values)
    if not peers:
        sys.exit(3)
    if len(peers) != 1:
        sys.exit(4)
    key = b'FILE_SERVER_PROXY_TOKEN='
    for value in peers[0]:
        if value.startswith(key):
            sys.stdout.buffer.write(value[len(key):])
            break
except PermissionError:
    sys.exit(5)
"#;

pub(super) fn read_command() -> Vec<String> {
    vec![
        "python3".into(),
        "-I".into(),
        "-S".into(),
        "-c".into(),
        READ_TOKEN.into(),
        shared_types::AGENT_FILE_SERVER_PORT.to_string(),
    ]
}

pub(super) fn credentials(
    result: ExecResult,
) -> Result<shared_types::FileServerRequestCredentials> {
    match result.exit_code {
        0 => Ok(shared_types::FileServerRequestCredentials {
            proxy_token: Some(result.stdout.trim().to_owned()).filter(|value| !value.is_empty()),
        }),
        3 => Err(Error::ManagementNotRunning),
        4 => Err(Error::Conflict(
            "Multiple embedded file peers require process reconciliation".into(),
        )),
        5 => Err(Error::ContainerExecError(
            "Permission denied reading embedded file peer credentials".into(),
        )),
        6 => Err(Error::ContainerExecError(
            "File listener is not the managed embedded agent peer".into(),
        )),
        // Do not include captured output: an exec failure may contain a secret.
        _ => Err(Error::ContainerExecError(format!(
            "Read embedded file peer credentials failed (exit {})",
            result.exit_code
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_token_is_explicit_and_read_failures_are_not_silently_unconfigured() {
        let result = |exit_code, stdout: &str| ExecResult {
            exit_code,
            stdout: stdout.into(),
            stderr: String::new(),
        };
        assert!(
            credentials(result(0, ""))
                .expect("unconfigured")
                .proxy_token
                .is_none()
        );
        assert_eq!(
            credentials(result(0, " effective-token "))
                .expect("effective")
                .proxy_token
                .as_deref(),
            Some("effective-token")
        );
        assert!(matches!(
            credentials(result(3, "")),
            Err(Error::ManagementNotRunning)
        ));
        for exit in [4, 5, 127] {
            assert!(credentials(result(exit, "must-not-be-logged")).is_err());
        }
    }
}
