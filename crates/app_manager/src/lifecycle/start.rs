//! start/restart 的统一部署+启动编排（从 app_ops 拆出）。
//!
//! [`start_app_enhanced`] 是 Java 的统一入口：无参数 = 传统启停；带 `url` 触发
//! 轻量部署（下载 zip → prepare → activate → 启动），失败语义对齐发布链
//! （activate 就绪失败保留旧版本现场 + Failed）。可选 env/idle/pg 顺带生效。

use tracing::{info, warn};

use crate::models::*;
use crate::release_flow::runtime::DEFAULT_READY_TIMEOUT_SECS;
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
        validate_app_id(app_id)?;

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
            Some(cred) => match self.align_start_pg(app_id, cred).await {
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
        validate_app_id(app_id)?;
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
        // owner 兜底注册（与 build 同款补记语义：显式传优先；workspace 接口是主注册
        // 来源）。失败仅告警——owner 缺失不影响部署本身（URL 拼接归属是消费侧问题）。
        if let Some(user_id) = request
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && let Err(e) = self.record_dev_registration(app_id, user_id).await
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
            Some(cred) => match self.align_start_pg(app_id, cred).await {
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
    /// 运行容器（config-hash 变更 → Recreate 换 Pod）→ 等就绪 → 包内 SQL 执行。
    ///
    /// 下载/解压/换 code 由容器内 app-cli 部署段完成（sha256 校验、marker 幂等
    /// 重启不重下载、上一代保留 `/app/.previous`）；失败 = supervisord 重试耗尽 →
    /// readiness 超时由 `wait_app_ready` 上报，code/ 现场不破坏（发布链失败语义
    /// 保持）。回滚 = 用旧制品 URL 重新 start。
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

        // 空 sha256 = 跳过校验（信任内网源；app-cli 侧同语义），仍参与 env 传递
        let sha256 = request
            .sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();

        info!(
            "[APP] start-deploy: app_id={app_id}, release_id={release_id}, url={url}, sha256_given={}",
            !sha256.is_empty()
        );

        // 1. PVC ensure（K8s；Docker no-op）
        self.ensure_app_workspace_ready(app_id, None).await?;

        // 2. env 组装：request.env 整段替换 or live 回退；剥离历史保留键（防误伤）
        // + 校验用户显式键（防伪造）；叠加部署三元组（权威覆盖业务同名键）。
        let mut env = match request.env.clone() {
            Some(e) => e,
            None => match self.runtime.get_app_container_spec(app_id).await {
                Ok(spec) => spec.env.unwrap_or_default(),
                Err(e) => {
                    // 首次部署（app 不存在）无 live 可回退；存在但读回失败不阻断——
                    // 部署三元组仍注入，业务 env 丢失由调用方重试自愈
                    tracing::warn!(
                        "[APP] start-deploy env live fallback failed (empty base): app_id={app_id}: {e}"
                    );
                    std::collections::HashMap::new()
                }
            },
        };
        crate::release_flow::identity::strip_release_identity(&mut env);
        crate::release_flow::identity::ensure_no_reserved_env(&env)?;
        env.insert("APP_DEPLOY_URL".to_string(), url.to_string());
        env.insert("APP_RELEASE_ID".to_string(), release_id.clone());
        env.insert("APP_DEPLOY_SHA256".to_string(), sha256);

        // 3. ensure/re-apply 运行容器：env 变更 → config-hash → Recreate rollout
        //    → 新 Pod 启动时 app-cli 部署段生效
        match self.get_app(app_id).await {
            Ok(_) => {
                // 已存在 → update 通道（env 显式整段替换，其余字段 live 回退）。
                // 镜像缺省 = 当前平台默认（RCODER_RUNTIME_IMAGE_DIGEST）——重新部署
                // 顺带收敛运行时镜像到最新配置（对齐"大升级全量更新"运维语义）。
                // 乐观锁跳过：部署是权威写，last-write-wins。
                let update = UpdateAppRequest {
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
                self.update_app(app_id, update).await?;
            }
            Err(AppOperationError::NotFound(_)) => {
                // 首次部署 → ensure 创建（镜像/端口/探针平台内定，env 携带部署三元组）；
                // user_id 走 metadata 回退（发布链无 user 上下文）
                let lock = self.acquire_process_release_lock(app_id).await;
                self.ensure_app_runtime(app_id, app_id, Some(env), None, lock)
                    .await?;
            }
            Err(e) => return Err(e),
        }

        // 4. 等就绪（下载/解压/起服务全在 readiness 窗口内，默认预算 300s）
        self.wait_app_ready(app_id, DEFAULT_READY_TIMEOUT_SECS)
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
        Ok((Some(release_id), sql_report))
    }

    /// 创建空容器（start 无 url 对不存在 app 的形态）：容器即基础设施——
    /// PG/ttyd/dbx 由镜像 supervisord 常驻，app-cli 进 idle 等部署（无
    /// release.lock → 探针应答 + 等 Pod 被部署动作替换）。后续 `start{url}`
    /// 走 update 通道注入部署三元组 → Recreate 换 Pod 完成部署，双 PVC
    /// 数据面（含空容器阶段建的表）无缝承接。
    ///
    /// 不调 wait_app_ready：空容器无应用就绪概念，readiness 由 idle app-cli
    /// 秒级应答，get_app 很快转 Running。
    async fn ensure_empty_runtime(&self, app_id: &str, request: &StartAppRequest) -> AppResult<()> {
        // Docker 模式数据卷 bind 源 prod/{user_id}/data/{app_id} 依赖真实 user_id
        //（K8s 不消费，但统一要求——owner 分区语义单值）
        let user_id = request
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppOperationError::Validation(
                    "user_id is required when starting a non-existent app \
                     (empty app provisioning; existing apps start without it)"
                        .to_string(),
                )
            })?
            .to_string();

        info!(
            "[APP] provisioning empty app (infrastructure only, no deployment): \
             app_id={app_id}, user_id={user_id}"
        );

        // PVC ensure（K8s；Docker no-op）
        self.ensure_app_workspace_ready(app_id, None).await?;

        // env：整段替换（app 不存在无 live 可回退）；剥离历史保留键 + 校验；
        // **不注入部署三元组**——容器内 app-cli 据此判定未部署进 idle
        let mut env = request.env.clone().unwrap_or_default();
        crate::release_flow::identity::strip_release_identity(&mut env);
        crate::release_flow::identity::ensure_no_reserved_env(&env)?;

        // ensure 创建：ports=http:9080 + 探针=3010 与部署容器平台内定一致——
        // update 通道无权改 ports（恒 live 回退），空容器若缺 9080，后续部署的
        // 应用入口流量永久断流
        let lock = self.acquire_process_release_lock(app_id).await;
        self.ensure_app_runtime(app_id, app_id, Some(env), Some(user_id), lock)
            .await
    }

    /// env / idle 覆盖（对已存在 app；复用 update 的整段替换语义）。
    async fn apply_start_overrides(
        &self,
        app_id: &str,
        request: &StartAppRequest,
    ) -> AppResult<()> {
        if request.env.is_some() {
            let current = self.get_app(app_id).await?;
            let update = UpdateAppRequest {
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
    async fn align_start_pg(&self, app_id: &str, cred: &StartPgCredential) -> AppResult<()> {
        self.align_db_credentials(
            app_id,
            shared_types::AlignCredentialsRequest {
                app_id: app_id.to_string(),
                username: cred.username.clone(),
                password: cred.password.clone(),
            },
        )
        .await
        .map(|_| ())
    }
}

/// 自动生成 release_id（`rel-{yyMMddHHmmss}-{8 位随机}`；调用方未传标记时用）。
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
    use super::{generate_release_id, validate_release_id_fs_safe};
    use crate::AppServiceTrait;
    use crate::models::StartAppRequest;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

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

    fn with_runtime_image_env() {
        // ensure_app_runtime 读 env 决定默认镜像；同值重复 set 对并行测试无害
        //（edition 2024 set_var 为 unsafe：测试进程单线程 env 写入，无并发读取者）
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
            user_id: Some("u-empty".into()),
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
            .expect_err("missing user_id must be rejected");
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
                    user_id: None,
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
