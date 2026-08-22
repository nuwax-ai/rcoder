//! start/restart 的统一部署+启动编排（从 app_ops 拆出）。
//!
//! [`start_app_enhanced`] 是 Java 的统一入口：无参数 = 传统启停；带 `url` 触发
//! 轻量部署（下载 zip → prepare → activate → 启动），失败语义对齐发布链
//! （activate 就绪失败保留旧版本现场 + Failed）。可选 env/idle/pg 顺带生效。

use tracing::{info, warn};

use super::models::*;
use super::service::AppService;
use super::utils::*;

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
            // 传统启动（app 必须已存在——create 已从 REST 面移除，首次创建走发布链或 url 部署）
            self.start_app(app_id).await?;
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
        let runtime = self.get_app(app_id).await?;
        Ok(StartAppResult {
            runtime,
            release_id,
            pg_aligned,
            pg_error,
            sql_report,
        })
    }

    /// 轻量部署链：release_id 解析（自动生成或显式）→ prepare → activate。
    /// 返回生效的 release_id。
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

        // sha256 缺省时 prepare 需要一个占位值——看 prepare 的幂等键：sha256 参与
        // 既有记录比对。缺省校验场景用 url 内容哈希后填入（下载后真实值）；
        // 简化：sha256 未给时生成全零占位会让重试幂等失效——改为必经下载后校验
        // 通道不可绕过的前提下，用时间戳随机占位（同 release_id 重发即 409 提示
        // 显式传 sha256）。这里采用：未给 sha256 时跳过校验（占位 "" 不参与比对）
        let sha256 = request
            .sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");

        info!(
            "[APP] start-deploy: app_id={app_id}, release_id={release_id}, url={url}, sha256_given={}",
            !sha256.is_empty()
        );

        // prepare：下载 + 入库（release 不存在则创建；幂等键 release_id+sha256+size）
        self.prepare_release(
            app_id,
            PrepareReleaseRequest {
                release_id: release_id.clone(),
                url: url.to_string(),
                sha256: sha256.to_string(),
                size_bytes: None,
                retention: None,
            },
        )
        .await?;

        // activate：切流 + ensure 容器 + 等就绪（失败保留现场——发布链语义）
        let release = self.activate_release(app_id, &release_id, None).await?;
        if release.status != ReleaseStatus::Active {
            return Err(AppOperationError::InvalidState(format!(
                "activation failed: {}",
                release.failure_message.unwrap_or_default()
            )));
        }

        // 包内 database SQL 自动执行（原 publish 编排语义迁移：缺省开；单文件失败
        // 仅收集进 report 不阻断部署——SQL 幂等性由模板约定自带）
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
                command: None,
                env: request.env.clone(),
                secrets: None,
                resources: None,
                ports: None,
                health_check: None,
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

#[cfg(test)]
mod tests {
    use super::generate_release_id;

    #[test]
    fn release_id_shape_and_uniqueness() {
        let a = generate_release_id();
        let b = generate_release_id();
        assert!(a.starts_with("rel-"), "got {a}");
        assert_ne!(a, b);
        assert_eq!(a.len(), "rel-".len() + 12 + 1 + 8);
    }
}
