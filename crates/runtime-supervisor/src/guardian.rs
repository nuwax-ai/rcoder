//! The root guardian owns only the worker's real Child, never its entire tree.
//! Owned commands have separate guardians; another independently launched CLI
//! must survive this worker's exit (in particular Windows Job inheritance).
use crate::{
    TOKEN_ENV, WORKER_ENV,
    record::{self, GenerationPhase},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub remove_env: Vec<OsString>,
    #[serde(default)]
    pub external_cleanup_args: Option<Vec<OsString>>,
}
pub(crate) struct Child {
    pub process: tokio::process::Child,
    pub lease: Option<tokio::process::ChildStdin>,
}
pub(crate) async fn spawn(root: &Path, launch: &Launch) -> Result<Child> {
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .arg(crate::GUARDIAN_ARG)
        .arg(root)
        .stdin(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    // It is deliberately outside any CLI-wide kill-on-drop Job.
    let mut child = command
        .spawn()
        .context("spawn independent worker guardian")?;
    let mut lease = child.stdin.take().context("worker guardian pipe missing")?;
    let mut bytes = serde_json::to_vec(launch)?;
    ensure!(bytes.len() < 1024 * 1024, "worker launch frame too large");
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), lease.write_all(&bytes)).await??;
    Ok(Child {
        process: child,
        lease: Some(lease),
    })
}

pub(crate) async fn run(root: &Path) -> Result<i32> {
    // 启动首读纳入有界观察（收据可见性，2026-09-28）：monitor 在 spawn 本进程
    // 前刚写 Pending 收据并重写 supervisor 状态，共享挂载上 lock/generation/
    // owner 锁可能短暂不可见。每轮重新取锁（轮间释放）；phase 身份检查在观察
    // 之外——已消费/已撤销不是可见性问题，立即拒绝。
    let (_lock, mut generation) = process_utils::observe::observe(
        "root guardian startup",
        Duration::from_secs(3),
        || async {
            let lock = record::lock(&root.join("generation.lock"))?;
            let generation = record::generation(root)?;
            let scope = root
                .parent()
                .and_then(Path::parent)
                .context("worker scope missing")?;
            record::is_locked(&scope.join("owner.lock"))?;
            Ok(Some((lock, generation)))
        },
    )
    .await?;
    ensure!(
        generation.phase == GenerationPhase::Pending,
        "worker authorization already consumed"
    );
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        (&mut input)
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes),
    )
    .await??;
    ensure!(
        bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'),
        "invalid worker launch frame"
    );
    let launch: Launch = serde_json::from_slice(&bytes)?;
    // 准入读取纳入有界观察（收据可见性）：command-admission 由 monitor 在
    // spawn 本进程前初始化，共享挂载上可能短暂不可见/锁短暂忙碌；拒绝
    // （no longer accepts work）不在观察内。try_acquire 无内建等待。
    let gate = process_utils::observe::observe(
        "guardian command admission",
        Duration::from_secs(3),
        || async {
            let gate = process_utils::command_authority::Gate::try_acquire(root)?;
            gate.require_open()?;
            Ok(Some(gate))
        },
    )
    .await?;
    generation.phase = GenerationPhase::Running;
    record::save(&root.join("generation.json"), &generation)?;
    let mut command = tokio::process::Command::new(&launch.program);
    for name in &launch.remove_env {
        command.env_remove(name);
    }
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .env(WORKER_ENV, root)
        .env(TOKEN_ENV, &generation.token)
        .env(process_utils::command_authority::WORK_ROOT_ENV, root);
    let child = command.spawn();
    drop(gate);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            generation.error = Some(format!("spawn execution process: {error}"));
            generation.exit_code = Some(1);
            finish(root, &mut generation, &launch).await;
            return Err(error.into());
        }
    };
    let mut byte = [0u8; 1];
    generation.worker_pid = child.id();
    if let Err(error) = record::save(&root.join("generation.json"), &generation) {
        // Never lose the actual Child just because diagnostic persistence failed.
        tracing::warn!(%error, "could not record worker PID");
    }
    let mut result = tokio::select! {
        exit = child.wait() => exit,
        _ = input.read(&mut byte) => {
            // EOF (parent death or accepted stop) is tied to this exact Child.
            // Keep retrying with the handle if the OS has not finished stopping.
            loop {
                if let Err(error) = child.start_kill() {
                    generation.error = Some(format!("stop execution process: {error}"));
                }
                match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                    Ok(result) => break result,
                    Err(_) => { drop(record::save(&root.join("generation.json"), &generation)); }
                }
            }
        }
    };
    // A wait error is not root-exit evidence. Retain the handle until the OS
    // actually confirms exit; never publish Quiescent on a failed wait.
    while let Err(error) = result {
        generation.error = Some(format!("wait execution process: {error}"));
        drop(record::save(&root.join("generation.json"), &generation));
        drop(child.start_kill());
        tokio::time::sleep(Duration::from_millis(200)).await;
        result = child.wait().await;
    }
    generation.exit_code = result.as_ref().ok().and_then(|s| s.code());
    finish(root, &mut generation, &launch).await;
    Ok(generation.exit_code.unwrap_or(1))
}

async fn finish(root: &Path, generation: &mut record::Generation, launch: &Launch) {
    generation.phase = GenerationPhase::Draining;
    loop {
        let cleanup = async {
            process_utils::command_authority::Gate::try_acquire(root)?.close()?;
            process_utils::guardian::recover(root)?;
            // After root exit and every guardian's cleanup, SpawnPending local
            // receipts can only be registrations interrupted before spawn.
            settle_unspawned(root)?;
            process_utils::command_context::require_quiescent(&root.join("commands"))?;
            if let Some(args) = &launch.external_cleanup_args {
                record::save(&root.join("generation.json"), generation)?;
                let mut command = tokio::process::Command::new(&launch.program);
                command
                    .args(args)
                    .current_dir(&launch.cwd)
                    .stdin(Stdio::null());
                process_utils::command_authority::detach_command(&mut command);
                command
                    .env("RCODER_SUPERVISOR_CLEANUP_ROOT", root)
                    .env("RCODER_SUPERVISOR_CLEANUP_TOKEN", &generation.token);
                command.kill_on_drop(true);
                let output = tokio::time::timeout(Duration::from_secs(30), command.output())
                    .await
                    .context("external engine cleanup timed out")??;
                ensure!(
                    output.status.success(),
                    "external engine cleanup failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        }
        .await;
        match cleanup {
            Ok(()) => {
                generation.phase = GenerationPhase::Quiescent;
                match record::save(&root.join("generation.json"), generation) {
                    Ok(()) => return,
                    Err(error) => {
                        tracing::warn!(%error, "could not persist quiescent generation record");
                        generation.phase = GenerationPhase::Draining;
                    }
                }
            }
            Err(error) => {
                let message = format!("command cleanup pending: {error:#}");
                if generation.error.as_deref() != Some(&message) {
                    generation.error = Some(message);
                    if let Err(save_error) = record::save(&root.join("generation.json"), generation)
                    {
                        tracing::warn!(%save_error, "could not persist cleanup pending state");
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
fn settle_unspawned(root: &Path) -> Result<()> {
    let commands = root.join("commands");
    if !commands.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(commands)? {
        let path = entry?.path();
        let mut value: serde_json::Value = record::read(&path)?;
        ensure!(value["version"] == 1, "unknown command record version");
        if value["phase"] == "SpawnPending" {
            value["phase"] = "Quiescent".into();
            value["termination"] = "OwnerExitedBeforeSpawn".into();
            record::save(&path, &value)?;
        }
    }
    Ok(())
}
