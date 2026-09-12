//! start/restart 的统一部署+启动编排（从 app_ops 拆出）。
//!
//! [`start_app_enhanced`] 是 Java 的统一入口：无参数 = 传统启停；带 `url` 触发
//! 轻量部署（容器内下载/校验/解压 → 换 code → 编排启动），同步等待边界 =
//! 部署段完成（[`Self::wait_deploy_stage`]；服务启动结果异步可见）。失败 =
//! 部署段失败/等待超时，code/ 现场不破坏（旧制品 URL 重发即回滚）。
//! 可选 env/idle/pg 顺带生效。

use garde::Validate as _;
use tracing::{info, warn};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;
// record_dev_registration 是 AppServiceTrait 方法（trait impl 在 service.rs）
use crate::AppServiceTrait;

impl AppService {
    /// 统一部署+启动。
    ///
    /// 1. 带 url：release_id（显式或自动生成）→ prepare_release（sha256 可选校验）
    ///    → activate_release（切流+ensure 容器+等就绪；失败保留现场）
    ///    → env/idle 生效 → app 已 Running（activate 内含启动）
    /// 2. 无 url：传统 start_app（scale=1）；env/idle 仍可对已存在 app 生效
    /// 3. pg 凭据：部署/启动完成后对齐（scram 验证 → 不一致重置）——
    ///    失败不阻断（结果进响应 pg_aligned/pg_error，可重试）
    pub async fn start_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        validate_start_request(app_id, &request)?;
        let request = self.validate_hot_env(app_id, request).await?;

        let (release_id, sql_report) = if let Some(url) = request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.deploy_from_url(app_id, url, &request).await?
        } else {
            // 无 url 传统启动三态：app 已存在 → scale1 启动；不存在 → 创建空容器
            //（基础设施形态：PG/ttyd/dbx 常驻 + app-cli idle 等部署，容器内
            // 无应用——用户可先连库建表/开终端，后续 start{url} 部署承接）。
            match self.start_app(app_id).await {
                Ok(_) => {}
                Err(AppOperationError::NotFound(_)) => {
                    self.ensure_empty_runtime(app_id, &request).await?;
                }
                Err(e) => return Err(e),
            }
            (None, None)
        };

        // env / idle 对已存在 app 生效（整段替换，与 update 同语义）
        if request.env.is_some() || request.idle_timeout_seconds.is_some() {
            self.apply_start_overrides(app_id, &request).await?;
        }

        // PG 对齐（部署完成后 app Running，exec 通道可用）
        let (pg_aligned, pg_error) = match &request.pg {
            Some(cred) => match self
                .align_start_pg(app_id, request.user_id.trim(), cred)
                .await
            {
                Ok(()) => (Some(true), None),
                Err(e) => {
                    warn!(
                        "[APP] start pg align failed (deployment unaffected): app_id={app_id}: {e}"
                    );
                    (Some(false), Some(e.to_string()))
                }
            },
            None => (None, None),
        };

        self.invalidate_deploy_cache().await;
        let runtime = self.get_app(app_id).await?;
        Ok(StartAppResult {
            runtime,
            release_id,
            pg_aligned,
            pg_error,
            sql_report,
        })
    }

    /// restart 变体：先 stop 再走统一启动（部署语义同 start）。
    pub async fn restart_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        validate_start_request(app_id, &request)?;
        let request = self.validate_hot_env(app_id, request).await?;
        // 带 url 时 activate 自带 stop+切流，无需先 stop；仅传统 restart 走 stop+start
        if request.url.is_none() {
            self.restart_app(app_id).await?;
        }
        self.start_app_enhanced_finish(app_id, request).await
    }

    /// start_enhanced 的后半段（restart 复用：前置启停已处理）。
    async fn start_app_enhanced_finish(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        // owner 注册（显式必填；workspace 接口是主注册来源，此处兜底补记）。
        // 失败仅告警——owner 注册失败不影响部署本身（URL 拼接归属是消费侧问题）。
        if let Err(e) = self
            .record_dev_registration(app_id, request.user_id.trim())
            .await
        {
            warn!("[APP] start owner registration failed (ignored): app_id={app_id}: {e}");
        }
        let (release_id, sql_report) = if let Some(url) = request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.deploy_from_url(app_id, url, &request).await?
        } else {
            (None, None)
        };
        if request.env.is_some() || request.idle_timeout_seconds.is_some() {
            self.apply_start_overrides(app_id, &request).await?;
        }
        let (pg_aligned, pg_error) = match &request.pg {
            Some(cred) => match self
                .align_start_pg(app_id, request.user_id.trim(), cred)
                .await
            {
                Ok(()) => (Some(true), None),
                Err(e) => {
                    warn!(
                        "[APP] restart pg align failed (deployment unaffected): app_id={app_id}: {e}"
                    );
                    (Some(false), Some(e.to_string()))
                }
            },
            None => (None, None),
        };
        self.invalidate_deploy_cache().await;
        let runtime = self.get_app(app_id).await?;
        Ok(StartAppResult {
            runtime,
            release_id,
            pg_aligned,
            pg_error,
            sql_report,
        })
    }

    /// 轻量部署链（RBD 卷形态·容器中心化）：env 注入部署三元组 → ensure/re-apply
    /// 运行容器（config-hash 变更 → Recreate 换 Pod）→ 等部署段完成 → 包内 SQL 执行。
    ///
    /// 下载/解压/换 code 由容器内 app-cli 部署段完成（sha256 校验、marker 幂等
    /// 重启不重下载、上一代保留 `/app/.previous`）。同步等待边界 = 部署段完成
    /// （编排已启动，[`Self::wait_deploy_stage`]）——服务启动结果异步可见
    /// （readiness 探针照常摘流/恢复流量）。失败 = 部署段失败（容器侧 Failed
    /// 相位，error 透传）或等待超时，code/ 现场不破坏（发布链失败语义保持）。
    /// 回滚 = 用旧制品 URL 重新 start。
    #[allow(clippy::type_complexity)]
    async fn deploy_from_url(
        &self,
        app_id: &str,
        url: &str,
        request: &StartAppRequest,
    ) -> AppResult<(Option<String>, Option<DatabaseSqlReport>)> {
        let release_id = match request
            .release_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(id) => id.to_string(),
            None => generate_release_id(),
        };
        validate_release_id_fs_safe(&release_id)?;

        let sha256 = normalize_deploy_sha(request.sha256.as_deref())?;

        info!(
            "[APP] start-deploy: app_id={app_id}, release_id={release_id}, url={url}, sha256_given={}, mode={:?}",
            !sha256.is_empty(),
            request.deploy_mode.unwrap_or_default(),
        );

        // 0. 热部署分派：hot 且 app 已在跑 → 容器内 API 原地换应用（不换 Pod，
        //    PG/ttyd/dbx 不断连）；前置不满足自动落回下方换 Pod 权威链
        if request.deploy_mode == Some(DeployMode::Hot)
            && let Some(()) = self
                .try_deploy_via_container_api(
                    app_id,
                    url,
                    &release_id,
                    &sha256,
                    request.env.as_ref(),
                )
                .await?
        {
            return Ok((Some(release_id), None));
        }

        let operation = self.try_acquire_process_release_lock(app_id).await?;

        // The runtime provisions storage under the application operation lease.
        // Preparing it here would race with a concurrent purge before acquiring that lease.

        // 2. env 组装：request.env 整段替换 or live 回退；剥离历史保留键（防误伤）
        // + 校验用户显式键（防伪造）；叠加部署三元组（权威覆盖业务同名键）。
        let mut env = match request.env.clone() {
            Some(e) => e,
            None => match self.runtime.get_app_container_spec(app_id).await {
                Ok(spec) => spec.env.unwrap_or_default(),
                Err(e) => {
                    if self
                        .runtime
                        .get_deployment_status(app_id)
                        .await
                        .map_err(|error| {
                            AppOperationError::Backend(format!(
                                "read deployment before env fallback: {error}"
                            ))
                        })?
                        .is_some()
                    {
                        return Err(AppOperationError::Backend(format!(
                            "read live deployment env: {e}"
                        )));
                    }
                    std::collections::HashMap::new()
                }
            },
        };
        crate::release_flow::identity::strip_release_identity(&mut env);
        crate::release_flow::identity::ensure_no_reserved_env(&env)?;
        env.insert("APP_DEPLOY_URL".to_string(), url.to_string());
        env.insert("APP_RELEASE_ID".to_string(), release_id.clone());
        env.insert("APP_DEPLOY_SHA256".to_string(), sha256);
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        env.insert(
            shared_types::APP_DEPLOY_OPERATION_ID.into(),
            operation_id.clone(),
        );
        env.insert(
            shared_types::APP_DEPLOY_GENERATION_ID.into(),
            operation_id.clone(),
        );

        // 3. ensure/re-apply 运行容器：env 变更 → config-hash → Recreate rollout
        //    → 新 Pod 启动时 app-cli 部署段生效
        // user_id 直接落 create params（Docker 数据卷 bind 源 prod/{user_id}/data/{app_id}
        // 分区依据）——不依赖 finish 段的 owner 补记注册（fire-and-forget，失败仅告警，
        // 曾致首次部署回退查空 → runtime 兜底 app_id 出孤儿目录 prod/{app_id}/）。
        // 必填化后 metadata 回退路径退役。
        let explicit_user_id = request.user_id.trim().to_string();
        match self.get_app(app_id).await {
            Ok(_) => {
                // 已存在 → update 通道（env 显式整段替换，其余字段 live 回退）。
                // 镜像缺省 = 当前平台默认（RCODER_RUNTIME_IMAGE_DIGEST）——重新部署
                // 顺带收敛运行时镜像到最新配置（对齐"大升级全量更新"运维语义）。
                // The retained operation lease and runtime resourceVersion fence serialize the commit.
                let update = UpdateAppRequest {
                    user_id: request.user_id.clone(),
                    name: None,
                    image: None,
                    env: Some(env),
                    secrets: None,
                    resources: None,
                    tenant_id: None,
                    space_id: None,
                    expected_resource_version: None,
                    recycle_enabled: None,
                    idle_timeout_seconds: None,
                };
                self.update_app_with_guard(app_id, update, &operation)
                    .await?;
            }
            Err(AppOperationError::NotFound(_)) => {
                // 首次部署 → ensure 创建（镜像/端口/探针平台内定，env 携带部署三元组）
                self.ensure_app_runtime_with_guard(
                    app_id,
                    app_id,
                    Some(env),
                    Some(explicit_user_id),
                    &operation,
                )
                .await?;
            }
            Err(e) => return Err(e),
        }

        // 4. 同步等待边界 = 部署段完成（app-cli 状态机离开 Deploying，编排已
        //    启动）——不等用户服务启动/bridge 探活（服务起不起是用户代码域，
        //    readiness 探针照常摘流；返回成功 ≠ 立即接流量）。容器侧部署失败
        //    （下载 404/sha256 不匹配/解压损坏）在此同步上报并透传 error 文本。
        self.wait_deploy_stage(app_id, &operation_id, &operation)
            .await?;

        // 5. 包内 database SQL 自动执行（缺省开；单文件失败仅收集进 report 不阻断）
        let mut sql_report: Option<DatabaseSqlReport> = None;
        if request.auto_execute_sql.unwrap_or(true) {
            match self.execute_database_sql(app_id).await {
                Ok(report) => {
                    for rel in &report.executed {
                        info!("[APP] start-deploy database sql executed: {rel}");
                    }
                    for fail in &report.failed {
                        warn!("[APP] start-deploy database sql failed (ignored): {fail}");
                    }
                    info!(
                        "[APP] start-deploy database sql done: executed={}, failed={}",
                        report.executed.len(),
                        report.failed.len()
                    );
                    sql_report = Some(report);
                }
                Err(e) => {
                    warn!("[APP] start-deploy database sql stage failed (ignored): {e}");
                }
            }
        }
        operation.finish().await?;
        Ok((Some(release_id), sql_report))
    }

    /// 创建空容器（start 无 url 对不存在 app 的形态）：容器即基础设施——
    /// PG/ttyd/dbx 由镜像 supervisord 常驻，app-cli 进 idle 等部署（无
    /// release.lock → 探针应答 + 等 Pod 被部署动作替换）。后续 `start{url}`
    /// 走 update 通道注入部署三元组 → Recreate 换 Pod 完成部署，双 PVC
    /// 数据面（含空容器阶段建的表）无缝承接。
    ///
    /// 不同步等待部署段：空容器无应用就绪概念，readiness 由 idle app-cli
    /// 秒级应答，get_app 很快转 Running。
    async fn ensure_empty_runtime(&self, app_id: &str, request: &StartAppRequest) -> AppResult<()> {
        // Docker 模式数据卷 bind 源 prod/{user_id}/data/{app_id} 依赖真实 user_id
        //（K8s 不消费，但统一要求——owner 分区语义单值；请求侧已必填）
        let user_id = request.user_id.trim().to_string();

        info!(
            "[APP] provisioning empty app (infrastructure only, no deployment): \
             app_id={app_id}, user_id={user_id}"
        );

        // env：整段替换（app 不存在无 live 可回退）；剥离历史保留键 + 校验；
        // **不注入部署三元组**——容器内 app-cli 据此判定未部署进 idle
        let mut env = request.env.clone().unwrap_or_default();
        crate::release_flow::identity::strip_release_identity(&mut env);
        crate::release_flow::identity::ensure_no_reserved_env(&env)?;

        // ensure 创建：ports=http:9080 + 探针=3010 与部署容器平台内定一致——
        // update 通道无权改 ports（恒 live 回退），空容器若缺 9080，后续部署的
        // 应用入口流量永久断流
        let lock = self.acquire_process_release_lock(app_id).await?;
        self.ensure_app_runtime(app_id, app_id, Some(env), Some(user_id), lock)
            .await
    }

    async fn validate_hot_env(
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

    /// env / idle 覆盖（对已存在 app；复用 update 的整段替换语义）。
    async fn apply_start_overrides(
        &self,
        app_id: &str,
        request: &StartAppRequest,
    ) -> AppResult<()> {
        if request.env.is_some()
            && request
                .url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
        {
            let current = self.get_app(app_id).await?;
            let update = UpdateAppRequest {
                user_id: request.user_id.clone(),
                name: None,
                image: None,
                env: request.env.clone(),
                secrets: None,
                resources: None,
                tenant_id: None,
                space_id: None,
                expected_resource_version: current.resource_version.clone(),
                recycle_enabled: None,
                idle_timeout_seconds: None,
            };
            self.update_app(app_id, update).await?;
            info!("[APP] start env override applied: app_id={app_id}");
        }
        if let Some(idle) = request.idle_timeout_seconds {
            self.set_recycle_policy(
                app_id,
                RecyclePolicyRequest {
                    // start 内部链构造：user_id 取请求显式档（StartAppRequest
                    // .user_id 已是必填 String——分区归属值直传）
                    user_id: request.user_id.clone(),
                    recycle_enabled: Some(idle > 0),
                    idle_timeout_seconds: Some(idle),
                    wake_on_traffic: None,
                },
            )
            .await?;
            info!("[APP] start idle override applied: app_id={app_id}, idle={idle}s");
        }
        Ok(())
    }

    /// PG 对齐（start 语境：仅对结果分级，不阻断部署——与 db/align 接口同一核心）。
    async fn align_start_pg(
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
fn validate_start_request(app_id: &str, request: &StartAppRequest) -> AppResult<()> {
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
fn normalize_deploy_sha(sha: Option<&str>) -> AppResult<String> {
    let sha = sha.map(str::trim).unwrap_or("");
    if !sha.is_empty() && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(AppOperationError::Validation(
            "sha256 must contain exactly 64 hexadecimal characters".into(),
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

/// Request correlation identity; deliberately independent of artifact content.
fn generate_release_id() -> String {
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
        let svc = test_service(tmp.path(), runtime.clone());
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
        let svc = test_service(tmp.path(), runtime.clone());

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
        // 计数含创建后的 env override（apply_start_overrides 走 update 通道幂等
        // re-apply；真实 runtime 为 SSA patch 不触发 rollout，Mock 的 patch=create 同构）
        assert!(
            runtime.create_calls.load(Ordering::SeqCst) >= 1,
            "empty app must be created"
        );
        // 运行状态就位（get_app 可见）
        assert_eq!(
            result.unwrap().runtime.status,
            crate::models::AppStatus::Running
        );
        // owner 落 metadata：后续 Docker 数据卷分区（prod/{user}/data/{app}）依据
        assert_eq!(
            svc.get_app_owner("app-empty-1").await.as_deref(),
            Some("u-empty")
        );
        // 首次创建参数（apply_start_overrides 的 update re-apply 会追加第二次调用，
        // 取 history 首条）：ports 含平台内定 9080（缺它后续部署的应用入口断流）
        // + env 无部署三元组
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
        let svc = test_service(tmp.path(), runtime.clone());

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
        let svc = test_service(tmp.path(), runtime.clone());

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
        let service = test_service(root.path(), runtime.clone());
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
