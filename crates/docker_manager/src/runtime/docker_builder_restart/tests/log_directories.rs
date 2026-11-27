//! 替代 builder 的日志目录：哪些 bind 换成新目录、label 怎么记。
//!
//! 这里放夹具和换目录的用例。哪些挂载换、换源记录怎么核（`roles`）、登记与创建结果
//! （`claims`）、重启协议（`replacement`）各有自己的文件。

use super::*;

mod claims;
mod replacement;
mod roles;

// ─── 带日志挂载的重启 ───────────────────────────────────────────────────────

const AUDIT_TARGET: &str = "/app/audit-logs";

/// 一次带日志挂载的重启。`host` 是 daemon 看到的路径，`visible` 是同一批位置在 RCoder 进程里
/// 的样子；两边各是真实的目录，靠注入的挂载表对应起来。配置里的日志根
/// `/app/logs/container` 经这张表落在 `host/container` 下
struct LoggedRestart {
    root: tempfile::TempDir,
    workspace: tempfile::TempDir,
    template: BuilderRestartTemplate,
    archive: Archive,
    /// 原容器的日志目录（宿主机路径）
    old_directory: String,
    old_name: String,
}

impl LoggedRestart {
    fn new() -> Self {
        Self::build(false)
    }

    /// 原容器有两条指向日志根的挂载
    fn with_audit_log() -> Self {
        Self::build(true)
    }

    fn build(audit: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let container = "rcoder-app-builder-265";
        let old_name = format!("{container}-20251026080000");
        let host_root = root.path().join("host/container");
        let old_directory = host_root.join(&old_name);
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::write(old_directory.join("old.log"), b"old").unwrap();
        let old_directory = old_directory.to_string_lossy().into_owned();
        let workspace_target = workspace.path().to_string_lossy().into_owned();
        let mut volumes = vec![
            host_path_volume(WORKSPACE_TARGET, &workspace_target),
            host_path_volume(LOG_TARGET, &old_directory),
        ];
        let mut binds = vec![
            format!("{old_directory}:{LOG_TARGET}:rw,z"),
            format!("{workspace_target}:{WORKSPACE_TARGET}"),
        ];
        let mut env = vec![
            format!("CONTAINER_LOGS_DIR={LOG_TARGET}"),
            format!("CONTAINER_LOG_NAME={old_name}"),
            "PATH=/usr/bin".into(),
        ];
        if audit {
            let audit_name = format!("{container}-audit-20251026080000");
            let audit_directory = host_root.join(&audit_name);
            std::fs::create_dir_all(&audit_directory).unwrap();
            let audit_directory = audit_directory.to_string_lossy().into_owned();
            volumes.push(host_path_volume(AUDIT_TARGET, &audit_directory));
            binds.push(format!("{audit_directory}:{AUDIT_TARGET}"));
            env.push(format!("AUDIT_LOG_NAME={audit_name}"));
        }
        let template = template_for(volumes);
        let archive = archive_for(&template, binds, env);
        Self {
            root,
            workspace,
            template,
            archive,
            old_directory,
            old_name,
        }
    }

    fn runtime(&self, address: std::net::SocketAddr) -> DockerRuntime {
        let resolver = crate::HostPathResolver::from_mount_table(vec![(
            self.root.path().join("visible/logs"),
            self.root.path().join("host"),
        )])
        .unwrap();
        // 配置里的日志根是 RCoder 进程里的路径，经上面这张表落在 `host/container`
        let config = crate::DockerManagerConfig {
            log_reclaim_root: self.visible_root(),
            ..Default::default()
        };
        DockerRuntime::with_log_path_resolver(docker_manager_configured(address, config), resolver)
    }

    /// RCoder 进程里新建日志目录的位置
    fn visible_root(&self) -> PathBuf {
        self.root.path().join("visible/logs/container")
    }

    fn host_root(&self) -> PathBuf {
        self.root.path().join("host/container")
    }

    /// 日志根的配置盖住了工作区（旧容器、改过日志根的部署）：把工作区挪到宿主机那棵日志树
    /// 下面，归档里记的也跟着换
    fn move_workspace_under_log_root(&mut self) {
        let workspace = tempfile::tempdir_in(self.host_root()).unwrap();
        self.replace_workspace(&workspace.path().to_string_lossy());
        drop(std::mem::replace(&mut self.workspace, workspace));
    }

    /// 把日志挂载挪到日志根下更深处（`host/container/<directories>/<旧目录>`）
    fn nest_log_mount(&mut self, directories: &str) {
        let nested = self.host_root().join(directories);
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::rename(&self.old_directory, nested.join(&self.old_name)).unwrap();
        let previous = std::mem::replace(
            &mut self.old_directory,
            nested.join(&self.old_name).to_string_lossy().into_owned(),
        );
        for volume in &mut self.template.volumes {
            if volume.name == LOG_TARGET {
                volume.uid = self.old_directory.clone();
            }
        }
        let archived = format!("{previous}:{LOG_TARGET}");
        for bind in &mut self.archive.binds {
            if bind.starts_with(&archived) {
                *bind = format!("{}:{LOG_TARGET}:rw,z", self.old_directory);
            }
        }
    }

    fn replace_workspace(&mut self, target: &str) {
        for volume in &mut self.template.volumes {
            if volume.name == WORKSPACE_TARGET {
                volume.uid = target.to_owned();
            }
        }
        for bind in &mut self.archive.binds {
            if bind.ends_with(&format!(":{WORKSPACE_TARGET}")) {
                *bind = format!("{target}:{WORKSPACE_TARGET}");
            }
        }
    }

    /// 创建请求里各条日志挂载绑定的源：挂载点 → 宿主机路径
    fn bound_sources(
        &self,
        daemon: &ScriptedDaemon,
        request: usize,
    ) -> serde_json::Map<String, Value> {
        let body = daemon.body(request);
        let sources = body["Labels"][RESTART_LOG_DIR]
            .as_str()
            .and_then(|label| serde_json::from_str::<Value>(label).ok());
        sources
            .and_then(|sources| sources.as_object().cloned())
            .unwrap_or_else(|| panic!("no readable log directory label in {body}"))
    }

    /// 创建请求里日志挂载实际绑定的目录名
    fn bound_log_name(&self, daemon: &ScriptedDaemon, request: usize, target: &str) -> String {
        let binds = daemon.body(request)["HostConfig"]["Binds"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let marker = format!(":{target}");
        let source = binds
            .iter()
            .filter_map(Value::as_str)
            .find_map(|bind| bind.split_once(marker.as_str()).map(|(source, _)| source))
            .unwrap_or_else(|| panic!("no bind targets {target}: {binds:?}"));
        std::path::Path::new(source)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// 模板里的卷，其中 `sources` 里有的挂载点换成对应的新目录
    fn expected_volumes(&self, sources: &[(&str, &str)]) -> Vec<AppResourceIdentity> {
        self.template
            .volumes
            .iter()
            .map(
                |volume| match sources.iter().find(|(target, _)| *target == volume.name) {
                    Some((_, source)) => host_path_volume(&volume.name, source),
                    None => volume.clone(),
                },
            )
            .collect()
    }

    async fn restore(&self, daemon: &ScriptedDaemon) -> Result<Replacement> {
        self.restore_archive(daemon, self.archive.clone()).await
    }

    async fn restore_archive(
        &self,
        daemon: &ScriptedDaemon,
        archive: Archive,
    ) -> Result<Replacement> {
        self.runtime(daemon.address)
            .replace_builder_container(&self.template, &archive, original(&self.template))
            .await
    }
}

fn new_log_directory(restart: &LoggedRestart, name: &str) -> String {
    restart
        .host_root()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// 落在可回收日志根下的 bind 不照原样重放：替代容器绑定同级的新目录，挂载点和选项不动，
/// 其它 bind 原样；新目录记在 label 上。
#[tokio::test]
async fn the_replacement_gets_a_new_log_directory() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    assert_eq!(
        name.strip_prefix("rcoder-app-builder-265-").map(str::len),
        Some(14),
        "the new directory keeps the container name and takes a new timestamp: {name}"
    );
    assert_ne!(name, restart.old_name);
    let new_directory = new_log_directory(&restart, &name);
    let body = daemon.body(4);
    assert_eq!(
        body["HostConfig"]["Binds"],
        json!([
            format!("{new_directory}:{LOG_TARGET}:rw,z"),
            format!("{}:{WORKSPACE_TARGET}", restart.workspace.path().display()),
        ]),
        "the log bind keeps its target and options; the workspace bind is replayed verbatim"
    );
    let env: Vec<String> = body["Env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        env[..3],
        [
            format!("CONTAINER_LOGS_DIR={LOG_TARGET}"),
            format!("CONTAINER_LOG_NAME={name}"),
            "PATH=/usr/bin".to_string(),
        ],
        "CONTAINER_LOG_NAME follows the directory that is actually bound"
    );
    assert!(
        env[3].starts_with("RCODER_EXECUTION_DOMAIN="),
        "the execution domain stamp is appended: {env:?}"
    );
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({LOG_TARGET: new_directory})
            .as_object()
            .unwrap()
            .clone(),
        "a resumed restore learns the directory from this label"
    );
    // 新目录是 RCoder 经自己的挂载视图建的
    assert!(restart.visible_root().join(&name).is_dir());
    // 旧目录原样留着，留给日志清理器
    assert_eq!(
        std::fs::read_dir(std::path::Path::new(&restart.old_directory))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
}

/// 旧日志目录已经不在了（停得久，被日志清理器回收）照样能恢复：替代容器用的本来就是新目录。
#[tokio::test]
async fn a_missing_old_log_directory_does_not_block_the_restart() {
    let restart = LoggedRestart::new();
    std::fs::remove_dir_all(&restart.old_directory).unwrap();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    assert_eq!(
        std::fs::read_dir(restart.visible_root()).unwrap().count(),
        1,
        "the replacement still gets a new directory"
    );
}

/// 旧版本的归档：环境变量里是带变量的模板，和实际挂载点对不上。识别日志挂载看的是源路径，
/// 不靠环境变量，照样换成新目录。
#[tokio::test]
async fn a_log_mount_is_recognised_by_its_source_even_when_the_env_does_not_match() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.env[0] = "CONTAINER_LOGS_DIR=/app/container-logs/{timestamp}".into();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore_archive(&daemon, archive).await.unwrap();

    daemon.assert_followed(5);
    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let new_directory = new_log_directory(&restart, &name);
    assert_ne!(new_directory, restart.old_directory);
    assert!(restart.visible_root().join(&name).is_dir());
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
}

/// 一个容器可以有几条指向日志根的挂载：每一条都换成自己的新目录。
#[tokio::test]
async fn every_log_mount_into_the_log_root_gets_its_own_new_directory() {
    let restart = LoggedRestart::with_audit_log();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    let log = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let audit = restart.bound_log_name(&daemon, 4, AUDIT_TARGET);
    let log_directory = new_log_directory(&restart, &log);
    let audit_directory = new_log_directory(&restart, &audit);
    assert_ne!(log_directory, audit_directory);
    assert!(audit.starts_with("rcoder-app-builder-265-audit-"));
    assert!(restart.visible_root().join(&log).is_dir());
    assert!(restart.visible_root().join(&audit).is_dir());
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({
            LOG_TARGET: log_directory,
            AUDIT_TARGET: audit_directory,
        })
        .as_object()
        .unwrap()
        .clone()
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[
            (LOG_TARGET, &log_directory),
            (AUDIT_TARGET, &audit_directory)
        ])
    );
}

/// 源落在日志根下、却没有挂载见证的 bind：改不了它，原样重放又可能绑到旧目录，拒绝。
#[tokio::test]
async fn a_bind_into_the_log_root_without_a_mount_witness_is_refused() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.binds.push(format!(
        "{}/stray-20251026080000:/app/stray",
        restart.host_root().display()
    ));
    let daemon = scripted(absent_original()).await;

    let result = restart.restore_archive(&daemon, archive).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("lies under the reclaimable log root but has no mount witness")),
        "{result:?}"
    );
    daemon.assert_no_request();
    assert!(
        !restart.visible_root().exists(),
        "nothing is created for a refused restart"
    );
}

/// 归档里的 bind 与模板记的挂载对不上：找不到要改写的那一条，拒绝。
#[tokio::test]
async fn a_log_bind_that_does_not_match_the_witness_conflicts_and_creates_nothing() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.binds[0] = format!(
        "{}/other-20251026080000:{LOG_TARGET}",
        restart.host_root().display()
    );
    let daemon = scripted(absent_original()).await;

    let result = restart.restore_archive(&daemon, archive).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart log bind does not match the archived mount witness")),
        "{result:?}"
    );
    daemon.assert_no_request();
    assert!(!restart.visible_root().exists());
}

/// 新目录建不出来就恢复失败，不退回去绑定归档里的旧目录。
#[tokio::test]
async fn a_log_directory_that_cannot_be_created_fails_the_restart() {
    let restart = LoggedRestart::new();
    // 日志根在 RCoder 进程里的位置被一个普通文件占着
    std::fs::create_dir_all(restart.root.path().join("visible/logs")).unwrap();
    std::fs::write(restart.visible_root(), b"not a directory").unwrap();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Cannot create a new log directory for the replacement builder")),
        "{result:?}"
    );
    daemon.assert_followed(3);
    assert!(
        std::fs::read_dir(std::path::Path::new(&restart.old_directory))
            .unwrap()
            .count()
            == 1,
        "the archived log directory is left exactly as it was"
    );
}

/// 上一次中断前建好的替代容器被原样接受，不再建第二个；它用的日志目录从 label 里读。
#[tokio::test]
async fn a_resumed_replacement_is_accepted_with_the_log_directories_it_was_given() {
    let restart = LoggedRestart::with_audit_log();
    let log_directory = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let audit_directory =
        new_log_directory(&restart, "rcoder-app-builder-265-audit-20251026090000");
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: log_directory,
                        AUDIT_TARGET: audit_directory,
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(2);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[
            (LOG_TARGET, &log_directory),
            (AUDIT_TARGET, &audit_directory),
        ])
    );
}

/// 没有 label 的替代容器是旧版本建的：那时归档原样重放，它绑的就是归档里的目录。
#[tokio::test]
async fn a_resumed_replacement_from_an_older_version_is_verified_against_the_archive() {
    let restart = LoggedRestart::new();
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({ RESTORE: restart.template.archive.uid.clone() }),
            ),
        ),
    ])
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(2);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
    assert!(
        !restart.visible_root().exists(),
        "nothing is created for a replacement that already exists"
    );
}

/// label 里的目录得是旧目录的同级新目录，不是随便一条路径，也不能就是旧目录本身。
#[tokio::test]
async fn a_resumed_label_that_is_not_a_fresh_sibling_is_refused() {
    let restart = LoggedRestart::new();
    for given in [
        "/srv/elsewhere/rcoder-app-builder-265-20251026090000".to_owned(),
        restart.old_directory.clone(),
        "relative/rcoder-app-builder-265-20251026090000".to_owned(),
    ] {
        let daemon = scripted(vec![
            step("GET", ORIGINAL_UID, 404, no_such_container()),
            step(
                "GET",
                "rcoder-app-builder-",
                200,
                retained_replacement(
                    &restart.template,
                    json!({
                        RESTORE: restart.template.archive.uid.clone(),
                        RESTART_LOG_DIR: json!({LOG_TARGET: given}).to_string(),
                    }),
                ),
            ),
        ])
        .await;

        let result = restart.restore(&daemon).await;

        assert!(
            matches!(&result, Err(Error::Conflict(message)) if message.contains("unexpected log directory")),
            "{given}: {result:?}"
        );
        daemon.assert_followed(2);
    }
}
