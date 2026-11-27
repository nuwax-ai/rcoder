use super::*;

fn plan(mode: WorkspaceSyncMode) -> MigrationJobPlan {
    MigrationJobPlan {
        job_name: "migrate-u-1-full".into(),
        namespace: "ns".into(),
        image: "img".into(),
        download_rclone: false,
        src_pvc: "shared".into(),
        dst_pvc: "dst".into(),
        src_subpath: "u_1".into(),
        bwlimit: match mode {
            WorkspaceSyncMode::Full => Some("150M".to_string()),
            WorkspaceSyncMode::Incremental => None,
        },
        filters: shared_types::WORKSPACE_MIGRATION_FILTERS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        mode,
    }
}

#[test]
fn full_mode_args_carry_bwlimit_and_filters() {
    let args = plan(WorkspaceSyncMode::Full).rclone_args();
    assert!(args.starts_with("sync '/src/u_1' /dst --links --metadata --create-empty-src-dirs"));
    assert!(args.contains("--bwlimit '150M'"));
    assert!(args.contains("--filter '- .cache/' --filter '- .npm/_npx/'"));
}

#[test]
fn incremental_mode_omits_bwlimit() {
    let args = plan(WorkspaceSyncMode::Incremental).rclone_args();
    assert!(!args.contains("--bwlimit"));
    assert!(args.contains("--filter '- .cache/' --filter '- .npm/_npx/'"));
}

#[test]
fn every_data_derived_argument_is_shell_quoted() {
    // 引用后参数边界只由引号决定：subpath 里的空白/元字符不会被
    // bash 分词或当成重定向。set -f 只防 glob，不防这个。
    let mut p = plan(WorkspaceSyncMode::Full);
    p.src_subpath = "u 1;rm -rf /".to_string();
    p.bwlimit = Some("150M".to_string());
    p.filters = vec!["a b".to_string(), "it's".to_string()];
    let args = p.rclone_args();
    assert!(args.starts_with("sync '/src/u 1;rm -rf /' /dst"), "{args}");
    assert!(args.contains("--filter 'a b'"), "{args}");
    // 内嵌单引号必须闭合-转义-重开，不能直接把引号断掉
    assert!(args.contains("--filter 'it'\\''s'"), "{args}");
}

#[test]
fn src_subpath_whitelist_accepts_real_user_ids() {
    for id in ["u_1", "user-42", "ABC123", "a", "migrate-rbd-test-user-1"] {
        assert!(validate_src_subpath(id).is_ok(), "{id} 应通过");
    }
}

#[test]
fn src_subpath_whitelist_rejects_path_and_shell_escape() {
    for id in [
        "", " ", "../etc", "a/b", "a b", "u;id", "u$(id)", "u`id`", "u|cat", "u*", "..", ".",
        "u\n", "用户",
    ] {
        let error = validate_src_subpath(id).unwrap_err().to_string();
        assert!(
            error.contains("not a valid userId"),
            "'{id}' 必须以 userId 白名单为由拒绝: {error}"
        );
    }
    // 超长同样拒（规则与 file-server 同一份：1-64）
    let long = "u".repeat(65);
    assert!(validate_src_subpath(&long).is_err());
}

#[test]
fn job_script_disables_shell_glob() {
    // set -f 必须在 exec 前——否则 core.[0-9]* 被 bash 按 CWD 展开
    let script = job_script(&plan(WorkspaceSyncMode::Full));
    assert!(script.contains("set -f"));
    let f_pos = script.find("set -f").unwrap();
    let exec_pos = script.find("exec rclone").unwrap();
    assert!(f_pos < exec_pos);
}

#[test]
fn explicit_migration_image_skips_download() {
    let (image, download) =
        resolve_migration_image(Some("reg/rclone-migrate:1.76.0".into()), None).unwrap();
    assert_eq!(image, "reg/rclone-migrate:1.76.0");
    assert!(!download, "专用镜像内已有 rclone, 不该走下载");
}

#[test]
fn alt_image_is_explicit_opt_in_for_download() {
    let (image, download) =
        resolve_migration_image(None, Some("reg/agent-runner:0.1.479".into())).unwrap();
    assert_eq!(image, "reg/agent-runner:0.1.479");
    assert!(download, "alt 镜像内没有 rclone, 必须容器内下载");
}

#[test]
fn blank_env_values_are_not_configuration() {
    // 空串/纯空格 = 没配（K8s env 留空即是这种值），不能当成有效镜像
    assert!(resolve_migration_image(Some("   ".into()), None).is_err());
    assert!(resolve_migration_image(None, Some(String::new())).is_err());
}

#[test]
fn missing_image_fails_fast_instead_of_defaulting_to_test_registry() {
    let err = resolve_migration_image(None, None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("RCODER_MIGRATION_IMAGE"),
        "错误必须点名该设的 env, 便于运维直接照做: {msg}"
    );
    assert!(
        !msg.contains("nuwax-k8s-test"),
        "不得再把测试仓库镜像写进错误文案: {msg}"
    );
}

#[test]
fn explicit_image_wins_over_alt() {
    let (image, download) = resolve_migration_image(
        Some("reg/rclone-migrate:1.76.0".into()),
        Some("reg/agent-runner:0.1.479".into()),
    )
    .unwrap();
    assert_eq!(image, "reg/rclone-migrate:1.76.0");
    assert!(!download);
}

/// 在**真实 bash** 下执行生成的脚本，返回 rclone 实际收到的 argv 与脚本 stdout
/// （假 rclone 把每个参数打到 stdout）。没有 bash 的环境返回 `None`。
#[cfg(unix)]
fn argv_delivered_by_job_script(p: &MigrationJobPlan) -> Option<(Vec<String>, String)> {
    let probe = std::process::Command::new("bash")
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !probe.status.success() {
        panic!("bash --version 失败");
    }

    let dir = std::env::temp_dir().join(format!("rcoder-migrate-argv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("rclone");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         if [ \"$1\" = version ]; then echo rclone v0; exit 0; fi\n\
         for a in \"$@\"; do printf '<%s>\\n' \"$a\"; done\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(job_script(p))
        // PATH 只含我们的目录 + 系统默认：不继承测试进程可能被改过的 PATH
        .env(
            "PATH",
            format!("{}:/usr/local/bin:/usr/bin:/bin", dir.display()),
        )
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap();
    drop(std::fs::remove_dir_all(&dir));
    assert!(
        output.status.success(),
        "脚本应以 rclone 的退出码结束；stderr 见上"
    );

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let argv = stdout
        .lines()
        .filter(|line| line.starts_with('<'))
        .map(|line| line.trim_matches(['<', '>']).to_string())
        .collect();
    Some((argv, stdout))
}

/// 端到端：生成的脚本在**真实 bash** 下执行，rclone 收到的 argv 必须与
/// 逐参数的预期完全一致——数据里的空白 / 元字符 / 引号 / `$()` / glob
/// 既不得改变参数边界，也不得在 `exec` 之前被日志行提前解释。
/// 没有 bash 的环境跳过。
#[cfg(unix)]
#[test]
fn job_script_delivers_each_argument_as_one_argv_element() {
    let mut p = plan(WorkspaceSyncMode::Full);
    p.src_subpath = "u 1;rm -rf /".to_string();
    p.bwlimit = Some("1M".to_string());
    p.filters = vec![
        "a b".to_string(),
        "it's".to_string(),
        "$(id)".to_string(),
        "core.[0-9]*".to_string(),
        "`id`".to_string(),
    ];

    let Some((argv, stdout)) = argv_delivered_by_job_script(&p) else {
        eprintln!("skipping: bash not found");
        return;
    };
    assert_eq!(
        argv,
        vec![
            "sync",
            "/src/u 1;rm -rf /",
            "/dst",
            "--links",
            "--metadata",
            "--create-empty-src-dirs",
            "--transfers",
            "8",
            "--checkers",
            "8",
            "--bwlimit",
            "1M",
            "--filter",
            "a b",
            "--filter",
            "it's",
            "--filter",
            "$(id)",
            "--filter",
            "core.[0-9]*",
            "--filter",
            "`id`",
            "--stats-one-line",
            "--stats",
            "300s",
        ],
        "rclone 实际收到的 argv 与预期不一致；脚本输出：\n{stdout}"
    );
    // 日志行照常打印了这条命令（不是静默执行）
    assert!(stdout.contains("[migrate] exec:"), "{stdout}");
}

/// 过滤规则的顺序就是优先级（rclone 先匹配先生效），每条规则是一个 argv 元素。
/// 这里逐条钉死词表的字面内容：只排除两类纯缓存，node_modules 之类一律迁
/// （按目录名排除它们曾把 pnpm 的版本切换目录迁坏）。k8s/migration 脚本的用例
/// 钉的是同一段文本。
#[cfg(unix)]
#[test]
fn job_script_passes_the_filter_vocabulary_in_order() {
    let Some((argv, stdout)) = argv_delivered_by_job_script(&plan(WorkspaceSyncMode::Full)) else {
        eprintln!("skipping: bash not found");
        return;
    };
    let start = argv
        .iter()
        .position(|arg| arg == "--filter")
        .unwrap_or_else(|| panic!("没有 --filter：{argv:?}"));
    let end = start + 2 * shared_types::WORKSPACE_MIGRATION_FILTERS.len();
    assert_eq!(
        argv[start..end].chunks(2).collect::<Vec<_>>(),
        [["--filter", "- .cache/"], ["--filter", "- .npm/_npx/"]],
        "{stdout}"
    );
    assert_eq!(
        argv[end..],
        ["--stats-one-line", "--stats", "300s"],
        "{stdout}"
    );
}

/// Job 只跑 rclone，不访问 API server：不挂 ServiceAccount token（namespace 里
/// 的 SA token 能读到什么，挂进数据面 Job 就是多大的面），系统调用面收敛到
/// 运行时默认 seccomp profile。与沙箱 Pod 同款加固。
/// 读 Job 状态的瞬时失败要容忍，持续失败才放弃（连接重置、502 不在 kube 客户端
/// 的内置重试范围内；同步一跑几小时，API server 抖一下不该让它白跑）。
#[test]
fn transient_job_status_read_failures_are_tolerated_until_they_persist() {
    use std::time::{Duration, Instant};
    let tolerance = JOB_GET_ERROR_TOLERANCE;
    let t0 = Instant::now();
    let mut budget = GetErrorBudget::default();
    assert!(!budget.exhausted(t0, tolerance), "第一次失败只计时");
    assert!(!budget.exhausted(t0 + tolerance - Duration::from_secs(1), tolerance));
    assert!(
        budget.exhausted(t0 + tolerance, tolerance),
        "持续满时限才放弃"
    );

    // 中间成功过一次: 重新计时
    let mut budget = GetErrorBudget::default();
    assert!(!budget.exhausted(t0, tolerance));
    budget.recovered();
    assert!(!budget.exhausted(t0 + tolerance * 10, tolerance));
    assert!(budget.exhausted(t0 + tolerance * 11, tolerance));
}

/// 反例（31.45 实测）：不带传播策略删 Job，Pod 被孤儿化、永久留在命名空间里。
#[test]
fn deleting_the_job_cascades_to_its_pods() {
    assert_eq!(
        job_delete_params().propagation_policy,
        Some(kube::api::PropagationPolicy::Background)
    );
}

#[test]
fn job_pod_mounts_no_service_account_token_and_sets_seccomp_profile() {
    let job = build_job(&plan(WorkspaceSyncMode::Full));
    let pod = job
        .spec
        .as_ref()
        .expect("spec")
        .template
        .spec
        .as_ref()
        .expect("pod spec");
    assert_eq!(pod.automount_service_account_token, Some(false));
    let profile = pod
        .security_context
        .as_ref()
        .and_then(|context| context.seccomp_profile.as_ref())
        .expect("pod seccomp profile");
    assert_eq!(profile.type_, "RuntimeDefault");
    assert_eq!(profile.localhost_profile, None);
    assert_eq!(pod.service_account_name, None);
}

#[test]
fn job_name_is_a_valid_dns_label_for_every_accepted_user_id() {
    for id in ["6", "mig-u1", &"u".repeat(43)] {
        for mode in [WorkspaceSyncMode::Full, WorkspaceSyncMode::Incremental] {
            let name = migration_job_name(id, mode);
            assert!(
                name.len() <= 63
                    && name.starts_with("migrate-")
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{name}"
            );
        }
    }
}

/// 反例（部署评审）：这些 id 能通过标识符白名单，却拼不出合法的 Job 名或
/// 标签值——拖到 Job 创建时才被 apiserver 拒绝（422）就晚了。
#[test]
fn migration_identifier_rejects_ids_that_cannot_name_the_job() {
    for id in ["6", "mig-u1", "user_42", &"u".repeat(43)] {
        assert!(validate_migration_identifier(id).is_ok(), "{id}");
    }
    for id in ["ABC123", "-mig", "mig-", "mig_", &"u".repeat(44)] {
        let error = validate_migration_identifier(id).unwrap_err();
        assert!(
            matches!(error, ContainerRuntimeError::ConfigurationError(_)),
            "{id}: {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("cannot name a Kubernetes migration Job")
        );
    }
    // 路径段规则仍在：越出共享卷的值以它为由拒绝
    let error = validate_migration_identifier("../etc").unwrap_err();
    assert!(error.to_string().contains("not a valid userId"), "{error}");
}

fn pod_with_container_state(name: &str, state: serde_json::Value) -> Pod {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name},
        "status": {"containerStatuses": [{"name": "sync", "state": state}]}
    }))
    .unwrap()
}

/// Job 起不来时 active 恒为 1、failed 恒为 0，只看 Job 状态要等满 4 小时——
/// 期间 cutover 的 agent 停着、写门禁与编排锁都被占着。
#[test]
fn blocked_container_reports_only_waits_that_do_not_recover() {
    let pulling = pod_with_container_state(
        "migrate-u1-full-aaaaa",
        serde_json::json!({"waiting": {"reason": "ContainerCreating"}}),
    );
    // 首次拉取失败可能只是抖动, 重试即恢复: 不算
    let first_failure = pod_with_container_state(
        "migrate-u1-full-bbbbb",
        serde_json::json!({"waiting": {"reason": "ErrImagePull", "message": "timeout"}}),
    );
    let running = pod_with_container_state(
        "migrate-u1-full-ccccc",
        serde_json::json!({"running": {"startedAt": "2026-11-23T00:00:00Z"}}),
    );
    let empty = pod_with_container_state(
        "migrate-u1-full-ddddd",
        serde_json::json!({"terminated": {"exitCode": 1}}),
    );
    assert_eq!(
        blocked_container([&pulling, &first_failure, &running, &empty]),
        None
    );

    let backoff = pod_with_container_state(
        "migrate-u1-full-eeeee",
        serde_json::json!({"waiting": {
            "reason": "ImagePullBackOff",
            "message": "Back-off pulling image \"reg/rclone-migrate:1.76.0\""
        }}),
    );
    assert_eq!(
        blocked_container([&pulling, &backoff]),
        Some(
            "migrate-u1-full-eeeee/sync: ImagePullBackOff: Back-off pulling image \
             \"reg/rclone-migrate:1.76.0\""
                .to_string()
        )
    );
    let broken = pod_with_container_state(
        "migrate-u1-full-fffff",
        serde_json::json!({"waiting": {"reason": "CreateContainerError"}}),
    );
    assert_eq!(
        blocked_container([&broken]),
        Some("migrate-u1-full-fffff/sync: CreateContainerError".to_string())
    );
}

/// 单次观测可能是瞬时状态：退避状态要累计到"满 20 秒且至少两次"才确认。
#[test]
fn stuck_observation_needs_time_and_two_hits_in_backoff() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let stuck_for = Duration::from_secs(20);
    let mut stuck = StuckObservation::default();

    // 一直在退避: 第一次只记录, 满 20 秒才确认
    assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
    assert!(!stuck.confirmed(
        PodBlockage::Blocked,
        t0 + Duration::from_secs(19),
        stuck_for
    ));
    assert!(stuck.confirmed(
        PodBlockage::Blocked,
        t0 + Duration::from_secs(20),
        stuck_for
    ));

    // 只命中一次、之后一直只是"在重试": 不确认（至少两次命中）
    let mut stuck = StuckObservation::default();
    assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
    assert!(
        !stuck.confirmed(
            PodBlockage::Retrying,
            t0 + Duration::from_secs(600),
            stuck_for
        ),
        "重试的一瞬既不计也不清零"
    );
    assert!(stuck.confirmed(
        PodBlockage::Blocked,
        t0 + Duration::from_secs(605),
        stuck_for
    ));

    // 没有异常: 清零重来
    let mut stuck = StuckObservation::default();
    assert!(!stuck.confirmed(PodBlockage::Blocked, t0, stuck_for));
    assert!(!stuck.confirmed(PodBlockage::None, t0 + Duration::from_secs(5), stuck_for));
    assert!(!stuck.confirmed(
        PodBlockage::Blocked,
        t0 + Duration::from_secs(60),
        stuck_for
    ));
}

/// 反例（31.45 实测）：kubelet 在退避与重试之间来回切——`ImagePullBackOff`
/// → `ErrImagePull` → `ImagePullBackOff`。旧判据要求连续两次同一个原因，
/// 中间夹一次重试就清零，100 秒才判出来。
#[test]
fn a_retry_between_two_backoffs_does_not_reset_the_stuck_detection() {
    use std::time::{Duration, Instant};
    let stuck_for = Duration::from_secs(20);
    let t0 = Instant::now();
    let backoff = pod_with_container_state(
        "migrate-u1-full-aaaaa",
        serde_json::json!({"waiting": {"reason": "ImagePullBackOff", "message": "Back-off pulling image"}}),
    );
    let retrying = pod_with_container_state(
        "migrate-u1-full-aaaaa",
        serde_json::json!({"waiting": {"reason": "ErrImagePull", "message": "manifest unknown"}}),
    );
    let pulling = pod_with_container_state(
        "migrate-u1-full-aaaaa",
        serde_json::json!({"waiting": {"reason": "ContainerCreating"}}),
    );
    assert_eq!(pod_blockage([&pulling]), PodBlockage::None);
    assert_eq!(pod_blockage([&retrying]), PodBlockage::Retrying);
    assert_eq!(pod_blockage([&backoff]), PodBlockage::Blocked);

    // 每 5 秒一次的真实节奏: 10s 退避, 15s 正在重试, 20s 退避, ... 30s 确认
    let mut stuck = StuckObservation::default();
    let timeline = [
        (10, &backoff),
        (15, &retrying),
        (20, &backoff),
        (25, &backoff),
        (30, &backoff),
    ];
    let mut confirmed_at = None;
    for (secs, pod) in timeline {
        if stuck.confirmed(
            pod_blockage(std::iter::once(pod)),
            t0 + Duration::from_secs(secs),
            stuck_for,
        ) && confirmed_at.is_none()
        {
            confirmed_at = Some(secs);
        }
    }
    assert_eq!(
        confirmed_at,
        Some(30),
        "中间夹一次重试不该把判定推到 100 秒以后"
    );
}

#[test]
fn job_spec_mounts_src_readonly() {
    let job = build_job(&plan(WorkspaceSyncMode::Full));
    let spec = job.spec.as_ref().expect("spec");
    assert_eq!(spec.backoff_limit, Some(2));
    let pod = spec.template.spec.as_ref().expect("pod spec");
    let src = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "src")
        .expect("src volume");
    assert_eq!(
        src.persistent_volume_claim.as_ref().unwrap().read_only,
        Some(true)
    );
    assert!(
        pod.containers
            .iter()
            .any(|c| c.name == "sync" && c.image.is_some())
    );
}

// ---- 补迁 Job ----

fn backfill_plan() -> BackfillJobPlan {
    BackfillJobPlan {
        job_name: backfill_job_name("u_1"),
        namespace: "ns".into(),
        image: "img".into(),
        download_rclone: false,
        src_pvc: "shared".into(),
        dst_pvc: "dst".into(),
        src_subpath: "u_1".into(),
        src_dir: "/src/u_1".into(),
        dst_dir: "/dst".into(),
        filters: shared_types::WORKSPACE_MIGRATION_FILTERS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        backfill: shared_types::WorkspaceBackfill::default(),
    }
}

#[test]
fn backfill_args_carry_source_destination_filters_then_only_patterns_then_paths() {
    let mut p = backfill_plan();
    p.backfill = shared_types::WorkspaceBackfill {
        paths: vec!["*".into(), "it's/proj node_modules".into()],
        only: vec!["node_modules/**".into(), "dist/**".into()],
    };
    assert_eq!(
        p.backfill_args(),
        "'/src/u_1' '/dst' '- .cache/' '- .npm/_npx/' '--' \
         '--only' 'node_modules/**' '--only' 'dist/**' '*' 'it'\\''s/proj node_modules'"
    );
    assert_eq!(
        backfill_job_name("u_1"),
        "migrate-u-1-backfill",
        "Job 名里下划线换连字符, 与同步 Job 同一条规则"
    );
}

/// 整段脚本放进引号 heredoc：内容必须以换行结尾（结束标记才独占一行）、且不能自己
/// 含有结束标记——任何一条不成立, 落成文件的脚本就会被截断。
#[test]
fn embedded_scripts_fit_the_heredoc_that_carries_them() {
    for script in [BACKFILL_SCRIPT, LINK_GUARD_SCRIPT] {
        assert!(script.ends_with('\n'), "脚本必须以换行结尾");
        assert!(
            !script
                .lines()
                .any(|line| line.trim_end() == SCRIPT_HEREDOC_END),
            "脚本内容不能出现 heredoc 的结束标记"
        );
    }
}

/// `rclone version | head -1` 在 pipefail 下有竞态：head 读完第一行就退出，rclone 余下的输出
/// 撞上 SIGPIPE，整个 Job 以 141 失败——那时它还什么都没做（拿真实 rclone 跑渲染出来的 Job
/// 时复现：5 次里 2 次）。sed 把输入读完，没有这个竞态。同步、补迁两种 Job 都走这一行。
#[test]
fn job_scripts_do_not_pipe_rclone_version_into_head() {
    for script in [
        job_script(&plan(WorkspaceSyncMode::Full)),
        backfill_job_script(&backfill_plan()),
    ] {
        assert!(script.contains("rclone version | sed -n 1p\n"), "{script}");
        assert!(!script.contains("rclone version | head"), "{script}");
    }
}

#[test]
fn backfill_report_is_read_from_the_markers_in_the_job_log() {
    let log = "rclone v1.76.0\n\
               [backfill] copy: .local\n\
               Transferred: 0 B / 0 B, --, -, 0 files, 0\n\
               [backfill] copied: .local\n\
               [backfill] copied: proj\n\
               [backfill] skipped: .nope\n\
               [verify] 软链完整性：核对软链 3 条，损坏 0 处\n";
    let report = parse_backfill_report(log);
    assert_eq!(report.copied, [".local", "proj"]);
    assert_eq!(report.skipped, [".nope"]);
    // rclone 的进度行、`copy:` 的开头行都不是报告
    let empty = Vec::<String>::new();
    assert_eq!(
        parse_backfill_report("[backfill] copied-x: .local").copied,
        empty
    );
    assert_eq!(log_tail("a\nb\nc\nd", 2), "c\nd");
    assert_eq!(log_tail("a\nb", 40), "a\nb");
    assert_eq!(log_tail("", 40), "");
}

#[test]
fn backfill_job_has_the_same_hardening_and_volumes_as_the_sync_job() {
    let p = backfill_plan();
    let job = build_backfill_job(&p);
    let pod = job.spec.unwrap().template.spec.unwrap();
    assert_eq!(job.metadata.name.unwrap(), "migrate-u-1-backfill");
    assert_eq!(pod.automount_service_account_token, Some(false));
    assert!(
        pod.security_context.is_some(),
        "seccomp 收敛到 RuntimeDefault"
    );
    let container = &pod.containers[0];
    assert_eq!(container.name, "backfill");
    let mounts = container.volume_mounts.as_deref().unwrap();
    assert_eq!(mounts[0].mount_path, "/src");
    assert_eq!(mounts[0].read_only, Some(true));
    assert_eq!(mounts[1].mount_path, "/dst");
    assert_eq!(mounts[1].read_only, Some(false));
    let volumes = pod.volumes.as_deref().unwrap();
    assert_eq!(
        volumes[0]
            .persistent_volume_claim
            .as_ref()
            .unwrap()
            .claim_name,
        "shared"
    );
    assert_eq!(
        volumes[1]
            .persistent_volume_claim
            .as_ref()
            .unwrap()
            .claim_name,
        "dst"
    );
}

/// 端到端：生成的 Job 脚本在**真实 bash** 下执行, 只有 rclone 是桩（记下 argv、按给定
/// 退出码退出）。其余都是真的：Job 脚本 → 渲染进去的 backfill.sh → 桩 rclone →
/// 渲染进去的 toolchain-guard.sh。没有 bash 的环境跳过。
#[cfg(unix)]
mod end_to_end;
