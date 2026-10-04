//! Bounded log reader contract tests, shared by both process entrypoints.
use workspace_manifest::LogSource;

use super::super::model::{LogSelector, MAX_SOURCES};
use super::super::read::MAX_LINE_BYTES;
use super::*;

fn release_lock() -> ReleaseLock {
    toml::from_str(
        r#"
schema_version = 1
release_id = "release-1"
workspace_name = "test"
minimum_app_cli_version = "0.1.0"
runtime_image_digest = "runtime:test"

[pingap]
mode = "managed"
version = "test"
commit = "test"

[[services]]
service_id = "api"
name = "API"
dir = "api"
type = "go"
kind = "web"
enabled = true
port = 18080

[services.run]
command = ["./api"]

[services.health]

[services.env]

[[services.logs]]
id = "application"
glob = "application*.log"
format = "jsonl"
"#,
    )
    .expect("valid release lock")
}

#[tokio::test]
async fn source_error_does_not_commit_partial_cursor_progress() {
    // 触发器说明：超长行已改为截断续读（见 oversized_line 测试），这里用
    // 非法 multiline 正则构造确定性的源读失败，验证坏源不产出日志、
    // 好源进度正常提交（增量续拉只返回好源新行）。
    let root = tempfile::tempdir().expect("log root");
    let api_dir = root.path().join("api");
    std::fs::create_dir_all(&api_dir).expect("api log directory");
    std::fs::create_dir_all(root.path().join("broken")).expect("broken log directory");
    let api_log = api_dir.join("application-1.log");
    std::fs::write(
        &api_log,
        "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n",
    )
    .expect("api log");
    std::fs::write(root.path().join("broken/application.log"), "line\n").expect("broken log");
    let mut release = release_lock();
    let mut broken = release.services[0].clone();
    broken.service_id = "broken".into();
    broken.name = "broken".into();
    broken.logs[0].format = LogFormat::Text;
    broken.logs[0].multiline_start_pattern = Some("[".into()); // 非法正则：读文件时编译失败
    release.services.push(broken);
    let service = LogService::new(release, root.path().to_path_buf());

    let initial = service
        .query(LogQueryRequest::default())
        .await
        .expect("initial query");
    assert_eq!(initial.logs.len(), 1);
    assert_eq!(initial.logs[0].message, "first");
    assert_eq!(initial.source_errors.len(), 1);
    assert_eq!(initial.source_errors[0].service_id, "broken");

    // 好源进度已提交：追加一行后增量续拉只返回新行（坏源仍报错、不产日志）。
    std::fs::write(
            &api_log,
            "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n{\"timestamp\":\"2026-08-03T00:00:02Z\",\"message\":\"new\"}\n",
        )
        .expect("append api log");
    let incremental = service
        .query(LogQueryRequest {
            cursor: Some(initial.cursor),
            ..Default::default()
        })
        .await
        .expect("incremental query");
    assert_eq!(incremental.logs.len(), 1);
    assert_eq!(incremental.logs[0].message, "new");
}

/// 超长行截断续读：旧行为 bail! 会毒化整源（cursor 不前进、每次重读撞
/// 同一行直到轮转走）；新行为产一条截断记录并越过该行。
#[tokio::test]
async fn oversized_line_is_truncated_instead_of_poisoning_the_source() {
    let root = tempfile::tempdir().expect("log root");
    let directory = root.path().join("api");
    std::fs::create_dir_all(&directory).expect("service log directory");
    let oversized = "x".repeat(MAX_LINE_BYTES + 10);
    let line_bytes = oversized.len() + 1; // 计入换行
    let mut contents = format!(
        "{{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"before\"}}\n{oversized}\n\
             {{\"timestamp\":\"2026-08-03T00:00:01Z\",\"message\":\"after\"}}\n"
    );
    std::fs::write(directory.join("application-1.log"), &contents).expect("log file");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());

    let snapshot = service
        .query(LogQueryRequest::default())
        .await
        .expect("snapshot query");
    assert!(
        snapshot.source_errors.is_empty(),
        "{:?}",
        snapshot.source_errors
    );
    assert_eq!(snapshot.logs.len(), 3);
    assert_eq!(snapshot.logs[0].message, "before");
    assert_eq!(snapshot.logs[1].message, "after");
    // 无时间戳的截断记录排最后（compare_timestamps 里 ts 为 None 恒排后）。
    let truncated = &snapshot.logs[2];
    assert!(
        truncated.message.starts_with("xxxx"),
        "{}",
        truncated.message
    );
    assert!(truncated.message.contains(&format!(
        "[app-cli truncated: original line was {line_bytes} bytes]"
    )));
    assert_eq!(truncated.timestamp, None);

    // cursor 已越过超长行：追加后增量续拉只返回新行。
    contents.push_str("{\"timestamp\":\"2026-08-03T00:00:02Z\",\"message\":\"new\"}\n");
    std::fs::write(directory.join("application-1.log"), &contents).expect("append log");
    let incremental = service
        .query(LogQueryRequest {
            cursor: Some(snapshot.cursor),
            ..Default::default()
        })
        .await
        .expect("incremental query");
    assert_eq!(incremental.logs.len(), 1);
    assert_eq!(incremental.logs[0].message, "new");
}

/// 行长恰为 MAX_LINE_BYTES+1 且以 \n 结尾：截断判定触发但行已完整读入，
/// 不得把下一行误吞进截断记录。
#[tokio::test]
async fn boundary_oversized_line_does_not_swallow_the_next_line() {
    let root = tempfile::tempdir().expect("log root");
    let directory = root.path().join("api");
    std::fs::create_dir_all(&directory).expect("service log directory");
    // 恰好 MAX+1 字节（含换行）的单行 + 前后各一条正常行。
    let boundary = format!("{}\n", "y".repeat(MAX_LINE_BYTES));
    assert_eq!(boundary.len(), MAX_LINE_BYTES + 1);
    std::fs::write(
        directory.join("application-1.log"),
        format!(
            "{{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"before\"}}\n{boundary}\
                 {{\"timestamp\":\"2026-08-03T00:00:01Z\",\"message\":\"after\"}}\n"
        ),
    )
    .expect("log file");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());

    let response = service
        .query(LogQueryRequest::default())
        .await
        .expect("query");
    assert!(
        response.source_errors.is_empty(),
        "{:?}",
        response.source_errors
    );
    assert_eq!(response.logs.len(), 3);
    assert_eq!(response.logs[0].message, "before");
    assert_eq!(response.logs[1].message, "after");
    assert!(response.logs[2].message.contains(&format!(
        "[app-cli truncated: original line was {} bytes]",
        MAX_LINE_BYTES + 1
    )));
}

/// 损坏 cursor（base64/JSON 解不开）自愈为全量重读，与 cursor_reset 契约一致。
#[tokio::test]
async fn undecodable_cursor_self_heals_as_reset() {
    use base64::Engine;
    let root = tempfile::tempdir().expect("log root");
    std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
    std::fs::write(
        root.path().join("api/application-1.log"),
        "{\"timestamp\":\"2026-08-03T00:00:00Z\",\"message\":\"first\"}\n",
    )
    .expect("log file");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());
    let not_base64 = "@@@not-base64@@@".to_string();
    let not_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode("not json")
        .to_string();
    for garbage in [not_base64, not_json] {
        let response = service
            .query(LogQueryRequest {
                cursor: Some(garbage),
                ..Default::default()
            })
            .await
            .expect("self-heal instead of 400");
        assert!(response.cursor_reset);
        assert_eq!(response.logs.len(), 1);
        assert_eq!(response.logs[0].message, "first");
    }
}

/// static 服务无进程，runtime.{out,err}.log 永不存在——不注入，避免
/// 全局查询/流恒定带 no-match 噪音。
#[tokio::test]
async fn static_service_is_not_injected_runtime_source() {
    let mut release = release_lock();
    release.services[0].r#type = workspace_manifest::ProjectType::Static;
    release.services[0].logs.clear();
    let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].service_id, "app-cli");
}

/// 零匹配（文件尚未写出/已被轮转清理）是正常状态，不是读失败——
/// 匹配可见性由 sources/query 的 matched_files=[] 承担。
#[tokio::test]
async fn zero_matched_files_is_not_a_source_error() {
    let root = tempfile::tempdir().expect("log root");
    std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());
    let response = service
        .query(LogQueryRequest::default())
        .await
        .expect("query");
    assert!(response.logs.is_empty());
    assert!(
        response.source_errors.is_empty(),
        "{:?}",
        response.source_errors
    );
    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    assert!(
        sources
            .iter()
            .any(|source| source.service_id == "api" && source.matched_files.is_empty())
    );
}

/// source_error 文案保留 anyhow 完整错误链（{:#}）——to_string() 只剩
/// 最外层 context，根因（非法正则等）会被吞掉，无法排障。
#[tokio::test]
async fn source_error_message_keeps_root_cause_chain() {
    let root = tempfile::tempdir().expect("log root");
    let directory = root.path().join("api");
    std::fs::create_dir_all(&directory).expect("service log directory");
    std::fs::write(directory.join("application-1.log"), "line\n").expect("log file");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let mut release = release_lock();
    release.services[0].logs[0].format = LogFormat::Text;
    release.services[0].logs[0].multiline_start_pattern = Some("[".into());
    let catalog = crate::LogCatalog::from_release(
        "test".into(),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
        release.clone(),
        LogLayout::Builtin,
    )
    .unwrap();
    let shared = LogService::from_catalog(catalog)
        .query(LogQueryRequest::default())
        .await
        .unwrap();
    let service = LogService::new(release, root.path().to_path_buf());
    let response = service
        .query(LogQueryRequest::default())
        .await
        .expect("query");
    // The management endpoint and stopped-service endpoint use the same reader,
    // including partial progress and full source error chains.
    assert_eq!(
        serde_json::to_value(&shared.logs).unwrap(),
        serde_json::to_value(&response.logs).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&shared.source_errors).unwrap(),
        serde_json::to_value(&response.source_errors).unwrap()
    );
    assert_eq!(response.source_errors.len(), 1);
    let message = &response.source_errors[0].message;
    assert!(message.contains("read source api/application"), "{message}");
    assert!(
        message.contains("compile multiline_start_pattern"),
        "{message}"
    );
    assert!(message.contains("regex parse error"), "{message}");
}

#[tokio::test]
async fn identical_catalog_keeps_cursor_across_reader_recreation() {
    let root = tempfile::tempdir().expect("log root");
    std::fs::create_dir_all(root.path().join("api")).expect("service log directory");
    std::fs::write(root.path().join("api/application.log"), "").expect("log file");
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let first = LogService::new(release_lock(), root.path().to_path_buf());
    let cursor = first
        .query(LogQueryRequest::default())
        .await
        .expect("first query")
        .cursor;
    let second = LogService::new(release_lock(), root.path().to_path_buf());
    let response = second
        .query(LogQueryRequest {
            cursor: Some(cursor),
            ..Default::default()
        })
        .await
        .expect("second query");
    assert!(!response.cursor_reset);
}

#[tokio::test]
async fn runtime_log_source_is_injected_when_service_declares_no_logs() {
    let mut release = release_lock();
    release.services[0].logs.clear();
    let service = LogService::new(release, PathBuf::from("/nonexistent-log-root"));
    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    // api/runtime + 内置 app-cli/orchestrator（空 selectors 全遍历）。
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].service_id, "api");
    assert_eq!(sources[0].source_id, "runtime");
    assert_eq!(sources[0].format, "text");
}

#[tokio::test]
async fn declared_runtime_keeps_its_contract_alongside_platform_stdout() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("api")).unwrap();
    std::fs::write(
        root.path().join("api/custom-runtime.log"),
        "{\"message\":\"user runtime source\"}\n",
    )
    .unwrap();
    std::fs::write(root.path().join("api/runtime.out.log"), "platform stdout\n").unwrap();
    let mut release = release_lock();
    release.services[0].logs = vec![LogSource {
        id: "runtime".into(),
        glob: "custom-runtime*.log".into(),
        format: LogFormat::Jsonl,
        multiline_start_pattern: None,
    }];
    let service = LogService::new(release, root.path().to_path_buf());
    let sources = service.sources(LogQueryRequest::default()).await.unwrap();
    assert_eq!(sources.len(), 3);
    assert!(sources.iter().any(|source| source.source_id == "runtime"
        && source.format == "jsonl"
        && source.matched_files == ["custom-runtime.log"]));
    assert!(
        sources
            .iter()
            .any(|source| source.source_id == "platform-runtime"
                && source.format == "text"
                && source.matched_files == ["runtime.out.log"])
    );
    let response = service.query(LogQueryRequest::default()).await.unwrap();
    assert!(
        response
            .logs
            .iter()
            .any(|log| log.source_id == "runtime" && log.message == "user runtime source")
    );
    assert!(
        response
            .logs
            .iter()
            .any(|log| log.source_id == "platform-runtime" && log.message == "platform stdout")
    );
    assert!(response.source_errors.is_empty());
}

#[tokio::test]
async fn injected_runtime_source_coexists_with_user_declared_sources() {
    // release_lock() 已声明 application 源；runtime 源应共存不覆盖，
    // 内置 app-cli/orchestrator 源并存。
    let service = LogService::new(release_lock(), PathBuf::from("/nonexistent-log-root"));
    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    assert_eq!(sources.len(), 3);
    let ids: Vec<&str> = sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect();
    assert!(ids.contains(&"application"), "{ids:?}");
    assert!(ids.contains(&"runtime"), "{ids:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn service_log_directory_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("log root");
    let outside = tempfile::tempdir().expect("outside directory");
    std::fs::write(outside.path().join("application.log"), "secret\n").expect("outside log");
    symlink(outside.path(), root.path().join("api")).expect("service directory symlink");
    // 内置 orchestrator 源匹配 log_root 根目录，须有真实文件避免其 error 混入断言。
    std::fs::write(root.path().join("app-cli.log.2026-01-01"), "").expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());

    let response = service
        .query(LogQueryRequest::default())
        .await
        .expect("query isolates source errors");
    assert!(response.logs.is_empty());
    // application 与平台注入的 runtime 两个源都命中 symlink 目录 → 各报一条 source error。
    assert_eq!(response.source_errors.len(), 2);
    assert!(
        response
            .source_errors
            .iter()
            .all(|error| error.message.contains("must be a real directory"))
    );
}

/// 内置编排器源：空 selectors 可见，文件匹配走 log_root 根目录特判，
/// JSON 行（文件层 JSON 化产物）解析出 timestamp/level/message。
#[tokio::test]
async fn orchestrator_source_matches_root_directory_glob() {
    let root = tempfile::tempdir().expect("log root");
    std::fs::write(
            root.path().join("app-cli.log.2026-08-31"),
            "{\"timestamp\":\"2026-08-31T00:00:00Z\",\"level\":\"INFO\",\"message\":\"🚀 start web\"}\n",
        )
        .expect("orchestrator log");
    let service = LogService::new(release_lock(), root.path().to_path_buf());

    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    let orchestrator = sources
        .iter()
        .find(|source| source.service_id == "app-cli")
        .expect("orchestrator source present");
    assert_eq!(orchestrator.source_id, "orchestrator");
    assert_eq!(orchestrator.format, "jsonl");
    assert!(
        orchestrator
            .matched_files
            .contains(&"app-cli.log.2026-08-31".to_string())
    );

    let response = service
        .query(LogQueryRequest {
            selectors: vec![LogSelector {
                service_id: "app-cli".into(),
                source_ids: Vec::new(),
            }],
            ..Default::default()
        })
        .await
        .expect("orchestrator query");
    let record = response
        .logs
        .iter()
        .find(|log| log.service_id == "app-cli")
        .expect("orchestrator record");
    assert_eq!(record.level.as_deref(), Some("INFO"));
    assert!(record.message.contains("🚀 start web"), "{record:?}");
}

/// A user service named app-cli retains its directory and source contract.
#[tokio::test]
async fn orchestrator_source_coexists_when_service_id_taken() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("app-cli")).unwrap();
    std::fs::write(
        root.path().join("app-cli/application.log"),
        "{\"message\":\"user application\"}\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("app-cli.log.2026-10-04"),
        "{\"message\":\"platform orchestrator\"}\n",
    )
    .unwrap();
    let mut release = release_lock();
    release.services[0].service_id = "app-cli".into();
    release.services[0].logs[0].id = "orchestrator".into();
    let service = LogService::new(release, root.path().to_path_buf());
    let response = service.query(LogQueryRequest::default()).await.unwrap();
    assert!(
        response
            .logs
            .iter()
            .any(|log| log.source_id == "orchestrator" && log.message == "user application")
    );
    assert!(
        response
            .logs
            .iter()
            .any(|log| log.source_id == "platform-orchestrator"
                && log.message == "platform orchestrator")
    );
    assert!(response.source_errors.is_empty());
}

/// idle 形态（空容器/未部署）仍注入编排器源——部署失败排障恰需此源。
#[tokio::test]
async fn idle_service_still_exposes_orchestrator_source() {
    let root = tempfile::tempdir().expect("log root");
    std::fs::write(root.path().join("app-cli.log.2026-08-31"), "{}\n").expect("log");
    let service = LogService::idle(root.path().to_path_buf());
    let sources = service
        .sources(LogQueryRequest::default())
        .await
        .expect("sources query");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].service_id, "app-cli");
    assert_eq!(sources[0].source_id, "orchestrator");
}

#[tokio::test]
async fn platform_registration_allocates_stable_suffix_and_keeps_user_bindings() {
    use crate::sources::PlatformSourceKind;
    let root = tempfile::tempdir().unwrap();
    let mut release = release_lock();
    release.services[0].logs = ["build", "platform-build"]
        .into_iter()
        .map(|id| LogSource {
            id: id.into(),
            glob: "application*.log".into(),
            format: LogFormat::Jsonl,
            multiline_start_pattern: None,
        })
        .collect();
    let mut service = LogService::new(release, root.path().to_path_buf());
    let source = LogSource {
        id: "build".into(),
        glob: "dev-*.log".into(),
        format: LogFormat::Text,
        multiline_start_pattern: None,
    };
    let id = service
        .add_platform_directory(
            "api",
            PlatformSourceKind::Build,
            source.clone(),
            root.path().join("build-one"),
        )
        .unwrap();
    assert_eq!(id, "platform-build-2");
    assert_eq!(
        service
            .add_platform_directory(
                "api",
                PlatformSourceKind::Build,
                source.clone(),
                root.path().join("build-two")
            )
            .unwrap(),
        id
    );
    let mut changed = source;
    changed.glob = "different.log".into();
    assert_eq!(
        service
            .add_platform_directory(
                "api",
                PlatformSourceKind::Build,
                changed,
                root.path().join("unrelated")
            )
            .unwrap(),
        "platform-build-3"
    );
    let sources = service.sources(LogQueryRequest::default()).await.unwrap();
    assert_eq!(
        sources
            .iter()
            .filter(|source| source.source_id == "platform-build-2")
            .count(),
        1
    );
    assert_eq!(
        sources
            .iter()
            .find(|source| source.source_id == "build")
            .unwrap()
            .format,
        "jsonl"
    );
}

#[tokio::test]
async fn user_named_platform_sources_do_not_exempt_business_quota() {
    let root = tempfile::tempdir().unwrap();
    let mut release = release_lock();
    release.services[0].service_id = "app-cli".into();
    release.services[0].logs = (0..=MAX_SOURCES)
        .map(|index| LogSource {
            id: format!("platform-build-{index}"),
            glob: "application*.log".into(),
            format: LogFormat::Jsonl,
            multiline_start_pattern: None,
        })
        .collect();
    let service = LogService::new(release, root.path().to_path_buf());
    assert!(service.sources(LogQueryRequest::default()).await.is_err());
}

#[tokio::test]
async fn runtime_only_services_still_consume_business_service_quota() {
    let root = tempfile::tempdir().unwrap();
    let mut release = release_lock();
    let mut template = release.services.remove(0);
    template.logs.clear();
    release.services = (0..=MAX_SERVICES)
        .map(|index| {
            let mut service = template.clone();
            service.service_id = format!("api-{index}");
            service
        })
        .collect();
    let service = LogService::new(release, root.path().to_path_buf());
    assert!(
        service.sources(LogQueryRequest::default()).await.is_err(),
        "platform stdout must not exempt business services from their original 64-service limit"
    );
}
