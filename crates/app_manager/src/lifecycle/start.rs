//! start/restart 的统一部署+启动编排（从 app_ops 拆出）。
//!
//! [`start_app_enhanced`] 是 Java 的统一入口：无参数 = 传统启停；带 `url` 触发
//! 轻量部署（容器内下载/校验/解压 → 换 code → 编排启动），同步等待边界 =
//! 部署段完成（[`Self::wait_deploy_stage`]；服务启动结果异步可见）。失败 =
//! 部署段失败/等待超时，code/ 现场不破坏（旧制品 URL 重发即回滚）。
//! 可选 env/idle/pg 顺带生效。

use garde::Validate as _;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    pub async fn start_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        self.deploy_controlled(app_id, request, false).await
    }
    pub async fn restart_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        self.deploy_controlled(app_id, request, true).await
    }
    pub(super) async fn validate_hot_env(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppRequest> {
        if request.deploy_mode == Some(DeployMode::Hot)
            && let Some(env) = &request.env
        {
            if self
                .runtime
                .get_deployment_status(app_id)
                .await
                .map_err(|error| {
                    AppOperationError::Backend(format!(
                        "read deployment before hot env validation: {error}"
                    ))
                })?
                .is_none()
            {
                return Ok(request);
            }
            let live = self
                .runtime
                .get_app_container_spec(app_id)
                .await
                .map_err(|error| {
                    AppOperationError::Validation(format!(
                        "cannot verify hot deployment env; use pod mode: {error}"
                    ))
                })?;
            if crate::release_flow::identity::business_env(env.clone())
                != crate::release_flow::identity::business_env(live.env.unwrap_or_default())
            {
                return Err(AppOperationError::HotDeployEnvChange(
                    "hot deployment cannot change business env; use pod mode".into(),
                ));
            }
            // Retain the requested env for the authoritative check under the hot lease.
        }
        Ok(request)
    }

    /// PG 对齐（start 语境：仅对结果分级，不阻断部署——与 db/align 接口同一核心）。
    pub(super) async fn align_start_pg(
        &self,
        app_id: &str,
        user_id: &str,
        cred: &StartPgCredential,
    ) -> AppResult<()> {
        self.align_db_credentials(
            app_id,
            shared_types::AlignCredentialsRequest {
                app_id: app_id.to_string(),
                user_id: user_id.to_string(),
                username: cred.username.clone(),
                password: cred.password.clone(),
            },
        )
        .await
        .map(|_| ())
    }
}

/// Validate both public entrypoints before stop, metadata writes or lease acquisition.
pub(super) fn validate_start_request(app_id: &str, request: &StartAppRequest) -> AppResult<()> {
    request.validate().map_err(|errors| {
        AppOperationError::Validation(
            errors
                .iter()
                .map(|(path, error)| format!("{path}: {}", error.message()))
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    validate_app_id(app_id)?;
    if request
        .url
        .as_deref()
        .is_some_and(|url| url.trim().is_empty())
    {
        return Err(AppOperationError::Validation(
            "Deployment url must not be empty".into(),
        ));
    }
    normalize_deploy_sha(request.sha256.as_deref())?;
    if let Some(release_id) = request
        .release_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        validate_release_id_fs_safe(release_id)?;
    }
    if let Some(env) = &request.env {
        crate::release_flow::identity::ensure_business_env(env)?;
    }
    Ok(())
}

/// Validate the entire digest before any deployment side effect or filesystem use.
pub(super) fn normalize_deploy_sha(sha: Option<&str>) -> AppResult<String> {
    let sha = sha.map(str::trim).unwrap_or("");
    if !sha.is_empty() && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(AppOperationError::Validation(
            "sha256 must contain exactly 64 hexadecimal characters".into(),
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

/// Request correlation identity; deliberately independent of artifact content.
pub(super) fn generate_release_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // pid ^ 时间戳旋转 ^ 原子计数器：同进程同秒连续生成不碰撞
    let rand = (std::process::id() as u64)
        ^ ts.rotate_left(17)
        ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    format!(
        "rel-{:012x}-{:08x}",
        ts & 0xffff_ffff_ffff,
        rand & 0xffff_ffff
    )
}

/// release_id 进容器 env 且 app-cli 侧拼 fs 路径（`.incoming/{id}`/`.staging/{id}`），
/// 白名单与 app-cli deploy 段同规则：`[A-Za-z0-9._-]+`、无前导点（rollout 前
/// fail fast，不等到容器内才报）。
fn validate_release_id_fs_safe(release_id: &str) -> AppResult<()> {
    let ok = !release_id.is_empty()
        && !release_id.starts_with('.')
        && release_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        return Err(AppOperationError::Validation(format!(
            "release_id must be [A-Za-z0-9._-]+ with no leading dot, got '{release_id}'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{generate_release_id, normalize_deploy_sha, validate_release_id_fs_safe};
    use crate::AppServiceTrait;
    use crate::models::StartAppRequest;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn blank_url_rejects_both_entrypoints_before_any_side_effect() {
        let directory = tempfile::tempdir().expect("test directory");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(directory.path(), runtime.clone()).await;
        for url in ["", " ", "\t\n"] {
            let request = StartAppRequest {
                user_id: "blank-owner".into(),
                url: Some(url.into()),
                ..Default::default()
            };
            for result in [
                service
                    .start_app_enhanced("blank-url", request.clone())
                    .await,
                service.restart_app_enhanced("blank-url", request).await,
            ] {
                assert!(matches!(
                    result,
                    Err(crate::models::AppOperationError::Validation(_))
                ));
            }
        }
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert!(
            service
                .metadata
                .store
                .get_application("blank-url")
                .await
                .expect("identity query")
                .is_none()
        );
        assert!(
            service
                .metadata
                .store
                .unfinished_operations(None, 10)
                .await
                .expect("operation query")
                .is_empty()
        );
        assert!(
            service.release_locks.is_empty(),
            "validation precedes operation locking"
        );
    }

    #[test]
    fn sha_validation_rejects_non_hex_and_unicode_without_slicing() {
        for invalid in [
            "a".repeat(15) + "é",
            "g".repeat(64),
            "a".repeat(16) + &"/".repeat(48),
            "a".repeat(63),
        ] {
            assert!(normalize_deploy_sha(Some(&invalid)).is_err());
        }
        assert_eq!(
            normalize_deploy_sha(Some(&"AB".repeat(32))).unwrap(),
            "ab".repeat(32)
        );
        assert_eq!(normalize_deploy_sha(None).unwrap(), "");
        assert_eq!(normalize_deploy_sha(Some("  ")).unwrap(), "");
    }

    #[tokio::test]
    async fn invalid_digest_and_reserved_identity_have_no_runtime_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let svc = test_service(tmp.path(), runtime.clone()).await;
        let invalid = StartAppRequest {
            user_id: "u-test".into(),
            url: Some("http://localhost/artifact.zip".into()),
            sha256: Some("a".repeat(15) + "é"),
            ..Default::default()
        };
        assert!(
            svc.start_app_enhanced("app-validation", invalid)
                .await
                .is_err()
        );
        let forged = StartAppRequest {
            user_id: "u-test".into(),
            env: Some(
                [(
                    shared_types::APP_DEPLOY_OPERATION_ID.into(),
                    "forged".into(),
                )]
                .into(),
            ),
            ..Default::default()
        };
        assert!(
            svc.start_app_enhanced("app-validation", forged)
                .await
                .is_err()
        );
        assert!(runtime.deployments.is_empty());
    }

    #[test]
    fn release_id_shape_and_uniqueness() {
        let a = generate_release_id();
        let b = generate_release_id();
        assert!(a.starts_with("rel-"), "got {a}");
        assert_ne!(a, b);
        assert_eq!(a.len(), "rel-".len() + 12 + 1 + 8);
        assert!(validate_release_id_fs_safe(&a).is_ok());
    }

    #[test]
    fn release_id_fs_safe_rejects_traversal() {
        assert!(validate_release_id_fs_safe("../evil").is_err());
        assert!(validate_release_id_fs_safe("a/b").is_err());
        assert!(validate_release_id_fs_safe(".hidden").is_err());
        assert!(validate_release_id_fs_safe("rel-abc_1.2").is_ok());
    }

    // 豁免仅限测试 helper：edition-2024 env 变异是 unsafe；同值重复 set 对并行测试无害
    #[allow(unsafe_code)]
    fn with_runtime_image_env() {
        unsafe {
            std::env::set_var(
                "RCODER_RUNTIME_IMAGE_DIGEST",
                "registry.test/app-runtime:ut",
            );
        }
    }

    /// start 无 url 对不存在的 app → 创建空容器（三态之一）：
    /// create 恰好一次；owner 落 metadata（user_id 分区依据）；env 无部署三元组。
    #[tokio::test]
    async fn start_no_url_creates_empty_app_when_missing() {
        with_runtime_image_env();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let svc = test_service(tmp.path(), runtime.clone()).await;

        let request = StartAppRequest {
            user_id: "u-empty".into(),
            env: Some([("APP_FOO".to_string(), "1".to_string())].into()),
            ..Default::default()
        };
        let result = svc.start_app_enhanced("app-empty-1", request).await;
        assert!(
            result.is_ok(),
            "empty app provisioning must succeed: {result:?}"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            1,
            "initial env must not cause a second create or patch"
        );
        assert_eq!(
            runtime
                .create_params_history
                .get("app-empty-1")
                .expect("creation history")
                .len(),
            1
        );
        // 运行状态就位（get_app 可见）
        assert_eq!(
            result.unwrap().runtime.status,
            crate::models::AppStatus::Running
        );
        // owner 落 metadata：后续 Docker 数据卷分区（prod/{user}/data/{app}）依据
        assert_eq!(
            svc.get_app_owner("app-empty-1")
                .await
                .expect("owner query")
                .as_deref(),
            Some("u-empty")
        );
        // Exactly one creation carries platform ports, business env and tokens;
        // no deployment identity is injected for an empty application.
        let params = runtime
            .create_params_history
            .get("app-empty-1")
            .and_then(|v| v.first().cloned())
            .expect("create params must be captured");
        let env = params.env.as_ref().expect("env captured");
        assert_eq!(env.get("APP_FOO").map(String::as_str), Some("1"));
        assert!(
            !env.contains_key("APP_DEPLOY_URL"),
            "empty app must NOT carry deploy env (app-cli idles on absence)"
        );
        let ports = params.ports.as_ref().expect("ports captured");
        assert!(
            ports.iter().any(|p| p.port == shared_types::APP_ENTRY_PORT),
            "platform entry port 9080 must be present (update channel cannot add it later)"
        );
    }

    /// start 无 url 对不存在的 app 且缺 user_id → 400（数据卷分区依赖），
    /// 不触发任何 create。
    #[tokio::test]
    async fn start_no_url_without_user_id_is_rejected() {
        with_runtime_image_env();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let svc = test_service(tmp.path(), runtime.clone()).await;

        let err = svc
            .start_app_enhanced("app-empty-2", StartAppRequest::default())
            .await
            .expect_err("empty user_id must be rejected");
        assert!(
            matches!(err, crate::error::AppOperationError::Validation(_)),
            "got {err:?}"
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    }

    /// 已存在 app 的 start 无 url → 传统启动（scale1），不重复创建。
    #[tokio::test]
    async fn start_no_url_existing_app_scales_without_create() {
        with_runtime_image_env();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-exist-1".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-exist-1".into(),
                replicas: 0,
                ready_replicas: 0,
                phase: "Stopped".into(),
                ..Default::default()
            },
        );
        let svc = test_service(tmp.path(), runtime.clone()).await;

        let result = svc
            .start_app_enhanced(
                "app-exist-1",
                StartAppRequest {
                    user_id: "u1".into(),
                    ..Default::default()
                },
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            0,
            "existing app start must reuse (scale), not create"
        );
    }
}

#[cfg(test)]
mod env_contract_tests {
    use super::*;
    use crate::test_support::{MockRuntime, test_service};
    use std::{collections::HashMap, sync::Arc};

    #[tokio::test]
    async fn hot_env_change_rejected_before_runtime_mutation_equal_env_retained_for_locked_check() {
        let root = tempfile::tempdir().expect("workspace");
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "env-app".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "env-app".into(),
                phase: "Running".into(),
                ..Default::default()
            },
        );
        runtime.specs.insert(
            "env-app".into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(HashMap::from([
                    ("BUSINESS".into(), "A".into()),
                    ("APP_CLI_DEPLOY_TOKEN".into(), "platform".into()),
                ])),
                ..Default::default()
            },
        );
        let service = test_service(root.path(), runtime.clone()).await;
        let request = |value: &str| {
            serde_json::from_value::<StartAppRequest>(serde_json::json!({
            "user_id": "test-user", "url": "http://127.0.0.1:1/artifact.zip", "deploy_mode": "hot", "env": {"BUSINESS": value}
        })).expect("request")
        };
        let error = service
            .start_app_enhanced("env-app", request("B"))
            .await
            .expect_err("reject before deployment");
        assert_eq!(
            error.code(),
            shared_types::error_codes::ERR_HOT_DEPLOY_ENV_CHANGE
        );
        assert_eq!(
            runtime
                .create_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            runtime
                .delete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let accepted = service
            .validate_hot_env("env-app", request("A"))
            .await
            .expect("same env");
        assert!(
            accepted.env.is_some(),
            "equal business env must be rechecked under the operation lease"
        );
    }
}
