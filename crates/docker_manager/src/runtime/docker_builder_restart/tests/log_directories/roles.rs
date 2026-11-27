//! 哪些挂载换、换源记录怎么核：持久数据（`/home/user` 下面）永不换源，哪怕它落在日志根里；
//! 换源记录里不能出现持久数据和不是日志挂载的挂载；记录里没有的挂载和归档一模一样。

use super::*;

/// 日志根的配置盖住了工作区（旧容器、改过日志根的部署）：工作区是持久数据，不换。替代容器
/// 绑的还是原来那个目录；日志挂载照换。工作区的源登记到替代容器建出来。
#[tokio::test]
async fn a_workspace_under_the_log_root_keeps_its_directory() {
    let mut restart = LoggedRestart::new();
    restart.move_workspace_under_log_root();
    std::fs::write(restart.workspace.path().join("keep"), b"data").unwrap();
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    let seen_at_create = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_at_create);
    let daemon = scripted_with_hook(
        creation_script(step("POST", "/containers/create", 200, created())),
        Arc::new(move |request: &str| {
            if request.starts_with("POST") && request.contains("/containers/create") {
                recorder
                    .lock()
                    .unwrap()
                    .extend(crate::log_directory::pending_bind_sources());
            }
        }),
    )
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let new_directory = new_log_directory(&restart, &name);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{workspace}:{WORKSPACE_TARGET}"))),
        "the workspace keeps its directory: {binds:?}"
    );
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({ LOG_TARGET: new_directory })
            .as_object()
            .cloned()
            .unwrap(),
        "the workspace is never recorded as replaced"
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
    // 工作区的源登记着：它落在日志根里，是清理器的过期候选
    let mut seen = seen_at_create.lock().unwrap().clone();
    seen.sort();
    let mut expected = vec![PathBuf::from(new_directory), PathBuf::from(&workspace)];
    expected.sort();
    assert_eq!(seen, expected);
    assert!(restart.workspace.path().join("keep").is_file());
}

/// 日志挂载在日志根下更深处：替代容器的新目录建在同一层，日志根下那第一级目录的修改时间
/// 不会自己变，清理器会一直把它当过期候选。恢复时把它刷新到现在，清理器删前的复查才看得出
/// "刚被用过"。
#[tokio::test]
async fn a_nested_log_directory_refreshes_the_directory_the_cleaner_scans() {
    let mut restart = LoggedRestart::new();
    restart.nest_log_mount("tenant/logs");
    // 清理器按 RCoder 进程里的路径扫日志根；先把那第一级目录做成早就过期的样子
    let scanned = restart.visible_root().join("tenant");
    std::fs::create_dir_all(scanned.join("logs")).unwrap();
    let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);
    std::fs::File::open(&scanned)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    restart.restore(&daemon).await.unwrap();

    let refreshed = std::fs::metadata(&scanned).unwrap().modified().unwrap();
    assert!(
        refreshed
            .duration_since(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .is_ok(),
        "the directory the cleaner scans must look recently used, not expired since {long_ago:?}: {refreshed:?}"
    );
    assert_eq!(
        std::fs::read_dir(scanned.join("logs")).unwrap().count(),
        1,
        "the replacement's new log directory lives next to where the old one was"
    );
}

/// 记录里只有一条日志挂载（另一条建替代容器的时候判不出来、原样重放了）：记录里有的按记录，
/// 没有的必须和归档一模一样。替代容器已经存在，它绑什么就是什么，不为难恢复。
#[tokio::test]
async fn a_mount_the_record_does_not_name_stays_on_its_archived_directory() {
    let restart = LoggedRestart::with_audit_log();
    let fresh = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
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
                        LOG_TARGET: new_log_directory(
                            &restart,
                            "rcoder-app-builder-265-20251026090000"
                        )
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
        restart.expected_volumes(&[(LOG_TARGET, &fresh)])
    );
}

/// label 里不能出现工作区：`/home/user` 下面是持久数据，永不换源。多出来的键会改写那条挂载的
/// 期望值，那正是这份核验要拦的。
#[tokio::test]
async fn a_resumed_label_that_names_the_workspace_is_refused() {
    let restart = LoggedRestart::new();
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
                        LOG_TARGET: new_log_directory(
                            &restart,
                            "rcoder-app-builder-265-20251026090000"
                        ),
                        WORKSPACE_TARGET: "/srv/elsewhere/workspace"
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("which holds persistent data and is never replaced")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// label 里出现了一条既不是日志挂载、也不在日志根里的挂载：换它没有依据，拒绝。
#[tokio::test]
async fn a_resumed_label_that_names_an_auxiliary_mount_is_refused() {
    let restart = LoggedRestart::new();
    let assets = tempfile::tempdir().unwrap();
    let assets_source = assets.path().to_string_lossy().into_owned();
    let mut template = restart.template.clone();
    template
        .volumes
        .push(host_path_volume("/app/assets", &assets_source));
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &template,
                json!({
                    RESTORE: template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: new_log_directory(&restart, "rcoder-app-builder-265-20251026090000"),
                        "/app/assets": assets.path().with_file_name("assets-elsewhere").to_string_lossy().into_owned(),
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let result = runtime
        .replace_builder_container(&template, &restart.archive, original(&template))
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("which is not a log mount")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 旧版本保留的停止容器还在、工作区落在日志根里：工作区的源在删旧容器**之前**就登记，一直到
/// 替代容器建出来。旧容器一删它就不在任何容器的挂载里了，到建替代容器之间还要确认镜像。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn data_under_the_log_root_is_registered_before_the_old_container_is_removed() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let mut restart = LoggedRestart::new();
    restart.move_workspace_under_log_root();
    // 真实归档里的卷按挂载点排好，和从容器读回的见证同序
    restart
        .template
        .volumes
        .sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    crate::runtime::docker_compute_receipt::save_stop(&restart.template.source)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let daemon = scripted_with_hook(
        vec![
            step(
                "GET",
                ORIGINAL_UID,
                200,
                retained_original(&restart.template),
            ),
            step("GET", "/images/", 200, image_inspect()),
            step("DELETE", ORIGINAL_UID, 204, json!({})),
            step("GET", ORIGINAL_UID, 404, no_such_container()),
            step("GET", "rcoder-app-builder-", 404, no_such_container()),
            step("GET", "/images/", 200, image_inspect()),
            step("GET", "/info", 200, daemon_info()),
            step("POST", "/containers/create", 200, created()),
        ],
        Arc::new(move |request: &str| {
            let deleting = request.starts_with("DELETE");
            let preparing_image = request.starts_with("GET") && request.contains("/images/");
            let creating = request.starts_with("POST") && request.contains("/containers/create");
            if deleting || preparing_image || creating {
                recorder.lock().unwrap().push((
                    request.split_whitespace().next().unwrap_or("").to_owned(),
                    crate::log_directory::pending_bind_sources(),
                ));
            }
        }),
    )
    .await;

    restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(8);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");
    for (request, sources) in seen.iter() {
        assert!(
            sources.contains(&PathBuf::from(&workspace)),
            "{request}: the data directory is nobody's mount once the old container is gone: {sources:?}"
        );
    }
}

/// 取不到挂载表时读换源记录：判不出来不拦。记录里有一条名字不带 `container-logs` 的挂载
/// （建替代容器的时候判它在日志根里，所以换了），续做照样认。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn without_a_mount_table_a_recorded_swap_is_accepted_for_a_mount_without_a_log_name() {
    let mut restart = LoggedRestart::new();
    let old_cache = restart.host_root().join("cache-old");
    std::fs::create_dir_all(&old_cache).unwrap();
    let old_cache = old_cache.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/app/cache", &old_cache));
    restart
        .archive
        .binds
        .push(format!("{old_cache}:/app/cache"));
    let fresh_log = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let fresh_cache = new_log_directory(&restart, "cache-new");
    let daemon = scripted(vec![
        // 自检从 ping 开始；测试进程不在容器里，拿不到自己的容器 ID，等于取不到挂载表
        step("GET", "_ping", 200, json!("OK")),
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
                        LOG_TARGET: fresh_log,
                        "/app/cache": fresh_cache,
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;
    let runtime = DockerRuntime::new(docker_manager_configured(
        daemon.address,
        crate::DockerManagerConfig {
            log_reclaim_root: restart.visible_root(),
            ..Default::default()
        },
    ));

    let replacement = runtime
        .replace_builder_container(
            &restart.template,
            &restart.archive,
            original(&restart.template),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("an unavailable mount table must not block the resume: {error:?}")
        });

    daemon.assert_followed(3);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh_log), ("/app/cache", &fresh_cache)])
    );
}
