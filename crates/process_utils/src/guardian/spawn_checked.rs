//! A failed handshake retains the original guardian and its output pipes.
use super::*;

pub struct SpawnAttemptFailure {
    pub source: anyhow::Error,
    pub child: Option<OwnedChild>,
}

/// Resident startup shares the caller's absolute budget. Even a timed-out
/// handshake returns its exact guardian/lease for authoritative cleanup.
pub async fn spawn_guarded_owner_checked_until(
    command: tokio::process::Command,
    scope: &crate::command_authority::ResidentScope,
    capture: bool,
    deadline: tokio::time::Instant,
) -> std::result::Result<OwnedChild, SpawnAttemptFailure> {
    let gate = scope
        .require_open_until(deadline)
        .await
        .map_err(|error| unstarted(error, None))?;
    let registered =
        register_guardian_under_admission(command, scope.root(), None, true, Some(gate))
            .map_err(|error| unstarted(error, None))?;
    if tokio::time::Instant::now() >= deadline {
        return Err(unstarted(
            anyhow::anyhow!("resident startup deadline expired before OS spawn"),
            Some(&registered.root),
        ));
    }
    let executable =
        std::env::current_exe().map_err(|error| unstarted(error.into(), Some(&registered.root)))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--native-command-guardian")
        .arg(&registered.root)
        .env(crate::command_authority::WORK_ROOT_ENV, scope.root())
        .stdin(Stdio::piped())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0200);
    let mut child = command
        .spawn()
        .map_err(|error| unstarted(error.into(), Some(&registered.root)))?;
    let mut lease = child.stdin.take();
    let started = async {
        let pipe = lease
            .as_mut()
            .context("resident guardian lease pipe missing")?;
        tokio::time::timeout_at(deadline, pipe.write_all(&registered.frame))
            .await
            .context("resident guardian frame deadline expired")??;
        wait_for_start(
            &mut child,
            &registered.root,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    }
    .await;
    let owned = OwnedChild::Guarded {
        child: Box::new(child),
        lease,
        root: registered.root,
        receipt_unavailable_since: None,
    };
    match started {
        Ok(()) => Ok(owned),
        Err(source) => Err(SpawnAttemptFailure {
            source,
            child: Some(owned),
        }),
    }
}

/// Return the actual attempted guardian after a startup acknowledgement error.
/// Callers must drain its pipes and confirm stop before releasing their leases.
/// Errors without a Child occur before any OS process was started by this call.
pub async fn spawn_owned_checked(
    command: tokio::process::Command,
    record: Option<&crate::command_context::CommandRecord>,
) -> std::result::Result<OwnedChild, SpawnAttemptFailure> {
    let root = match crate::command_context::CommandContext::current()
        .and_then(|context| context.journal_root)
    {
        Some(commands) => match commands.parent() {
            Some(parent) => Some(parent.to_owned()),
            None => return Err(unstarted(anyhow::anyhow!("work root missing"), None)),
        },
        None => crate::command_authority::current_root(),
    };
    let Some(work_root) = root else {
        return spawn_managed(command)
            .map(OwnedChild::Direct)
            .map_err(|source| unstarted(source, None));
    };
    let declared_root = crate::command_authority::current_root();
    let registered = register_guardian(
        command,
        &work_root,
        record.and_then(|record| record.path()),
        declared_root.as_deref(),
    )
    .await
    .map_err(|source| unstarted(source, None))?;
    let executable = std::env::current_exe().map_err(|source| {
        unstarted(
            anyhow::Error::new(source).context("resolve command guardian executable"),
            Some(&registered.root),
        )
    })?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--native-command-guardian")
        .arg(&registered.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if registered.managed {
        command.env(crate::command_authority::WORK_ROOT_ENV, &work_root);
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0200);
    let mut child = command.spawn().map_err(|source| {
        unstarted(
            anyhow::Error::new(source).context("spawn command guardian"),
            Some(&registered.root),
        )
    })?;
    let mut lease = child.stdin.take();
    let started = async {
        let pipe = lease.as_mut().context("guardian lease pipe missing")?;
        pipe.write_all(&registered.frame)
            .await
            .context("send guardian command")?;
        wait_for_start(&mut child, &registered.root, Duration::from_secs(10)).await
    }
    .await;
    let owned = OwnedChild::Guarded {
        child: Box::new(child),
        lease,
        root: registered.root,
        receipt_unavailable_since: None,
    };
    match started {
        Ok(()) => Ok(owned),
        Err(source) => Err(SpawnAttemptFailure {
            source,
            child: Some(owned),
        }),
    }
}

fn unstarted(mut source: anyhow::Error, root: Option<&Path>) -> SpawnAttemptFailure {
    if let Some(root) = root {
        // This exact receipt was registered, but OS spawn was never reached or
        // failed without a Child. Revoke authorization without inventing exit.
        let revoke = (|| -> Result<()> {
            let _authorization = lock(root)?;
            let mut receipt = read(root)?;
            ensure!(
                receipt.phase == "Pending",
                "unstarted guardian authorization changed"
            );
            receipt.phase = "Revoked".into();
            save(root, &receipt)?;
            confirm_command(root, &receipt)
        })();
        if let Err(error) = revoke {
            source = source.context(format!("revoke unstarted command authorization: {error:#}"));
        }
    }
    SpawnAttemptFailure {
        source,
        child: None,
    }
}
