//! UserApp 容器内 PostgreSQL 管理（从 service.rs 拆出，extension-impl）。
//!
//! 职责：`align_db_credentials`（凭据对齐，流程单头
//! `shared_types::align_pg_credentials`）与 exec 前置 `ensure_app_running`。
//! 改密/建库 HTTP 面已按拍板下线，统一走 rcoder 转发层
//! `/api/v1/userapp/db/{env}/*`——userapp_forward::db 的 env 双环境 +
//! username upsert + dbx 同步为超集实现。

use async_trait::async_trait;
use tracing::info;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

/// UserApp 运行容器 exec 通道（runtime exec → PgCommandRunner）。
struct RuntimeExecRunner<'a> {
    service: &'a AppService,
    app_id: &'a str,
}

#[async_trait]
impl shared_types::PgCommandRunner for RuntimeExecRunner<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        let r = self
            .service
            .runtime
            .exec(
                self.app_id,
                vec!["sh".to_string(), "-c".to_string(), command.to_string()],
            )
            .await
            .map_err(|e| format!("exec failed: {e}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: r.exit_code,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }
}

impl AppService {
    /// PG 凭据对齐（UserApp 运行容器内，prod 环境）：验证传入密码与账号当前密码
    /// 是否一致（TCP scram），不一致则本地 trust ALTER USER 重置并复验。
    /// 流程单头 [`shared_types::align_pg_credentials`]；密码不落日志。
    pub async fn align_db_credentials(
        &self,
        app_id: &str,
        req: shared_types::AlignCredentialsRequest,
    ) -> AppResult<shared_types::AlignCredentialsOutcome> {
        validate_app_id(app_id)?;
        // 路径参数与 body.app_id 双头：不一致即拒（防路由键与语义键分裂）
        if req.app_id != app_id {
            return Err(AppOperationError::Validation(format!(
                "path app_id {app_id} != body app_id {}",
                req.app_id
            )));
        }
        self.ensure_app_running(app_id).await?;
        let runner = RuntimeExecRunner {
            service: self,
            app_id,
        };
        let outcome = shared_types::align_pg_credentials(&runner, &req.username, &req.password)
            .await
            .map_err(|e| match e {
                // 调用方输入问题（非法标识符/角色不存在）→ Validation（400 语义）；
                // 容器侧执行失败 → Backend
                shared_types::AlignError::InvalidInput(m)
                | shared_types::AlignError::RoleMissing(m) => AppOperationError::Validation(m),
                shared_types::AlignError::Command { .. } => {
                    AppOperationError::Backend(e.to_string())
                }
            })?;
        info!(
            "[APP] PG credentials aligned (prod): app_id={}, username={}, reset_performed={}",
            app_id, req.username, outcome.reset_performed
        );
        Ok(outcome)
    }

    /// 确保 app 处于运行状态（exec 类容器内操作的前置）。
    ///
    /// Stopped/ScaledDown 时**自动唤醒**（activity registry 的 single-flight
    /// scale-up，hold-and-wait ≤ wake_timeout 默认 60s；与文件接口
    /// `ops::files` 同款模式）——"有请求即唤醒"平台语义。Timeout（app 仍在
    /// 后台启动，stopped 态保留下次可重试）/Failed(e) → InvalidState；随后
    /// `get_app` 兜 404（ensure_running 对不存在的 app 恒 AlreadyRunning 幻报，
    /// stopped 集合未命中不 scale）。
    ///
    /// 仅 `align_db_credentials` 消费（改密/建库 HTTP 面已统一走转发层，见模块注释）。
    /// validate_app_id 由调用方先做（Fail Fast：参数校验在 runtime 调用前）。
    async fn ensure_app_running(&self, app_id: &str) -> AppResult<()> {
        use shared_types::AppWakeControl;
        use shared_types::PgCommandRunner as _;
        match self.activity.ensure_running(app_id).await {
            shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
            shared_types::WakeOutcome::Timeout => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} wake timeout (still starting in background), retry later"
                )));
            }
            shared_types::WakeOutcome::Failed(e) => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} wake failed: {e}"
                )));
            }
        }
        self.get_app(app_id).await?;
        // 容器内 PG 就绪等待（唤醒后 initdb/PG 启动窗口内 exec psql 会撞竞态）；
        // AlreadyRunning 时 pg_isready 首轮即过，开销一次探测。
        let runner = RuntimeExecRunner {
            service: self,
            app_id,
        };
        let wait = runner
            .run(&shared_types::pg_utils::pg_wait_ready_cmd(60))
            .await
            .map_err(AppOperationError::Backend)?;
        if wait.exit_code != 0 {
            return Err(AppOperationError::InvalidState(format!(
                "app {app_id} postgres not ready after wake: {}",
                wait.stderr.trim()
            )));
        }
        Ok(())
    }

    /// database 目录 SQL 自动执行（部署就绪后调用；失败仅收集不阻断）。
    ///
    /// 目录列举经容器内 `find`（RBD 卷 rcoder 不可挂载；SQL 内容执行本就在
    /// 容器内 psql，列举同侧天然一致）：
    /// 1. code 根 `database/`（建库/扩展类，先执行）
    /// 2. 各一级子项目 `{dir}/database/`（目录名排序）
    ///
    /// 目录内按文件名升序。逐文件 exec 容器内 `psql -f`（容器内路径
    /// `{app_code_root(app_id)}/{rel}`，`ON_ERROR_STOP=on` 单文件原子性），单文件失败
    /// 收集进 report 继续下一文件。find 输出逐段过
    /// [`is_shell_safe_path_component`] 白名单（防恶意文件名注入 `sh -c` 命令行）。
    pub async fn execute_database_sql(&self, app_id: &str) -> AppResult<DatabaseSqlReport> {
        validate_app_id(app_id)?;
        // 容器内代码根（workspace 压平挂载 /home/user/{app_id} 之下）
        let code_root = shared_types::paths::app_code_root(app_id);
        // 根 database 先、子项目后（-mindepth 3 排除根目录自身）
        let find_cmd = format!(
            "find {code_root}/database -maxdepth 1 -type f -name '*.sql' 2>/dev/null | sort; \
             find {code_root} -mindepth 3 -maxdepth 3 -type f -path '*/database/*.sql' 2>/dev/null | sort"
        );
        let output = self
            .runtime
            .exec(app_id, vec!["sh".to_string(), "-c".to_string(), find_cmd])
            .await
            .map_err(|e| map_runtime_error("[APP] database sql scan exec failed", e))?;

        let code_prefix = format!("{code_root}/");
        let mut files: Vec<String> = Vec::new();
        let mut scan_errors: Vec<String> = Vec::new();
        if output.exit_code != 0 {
            // "失败不阻断"契约：扫描失败收集进 report（executed 空）继续返回
            scan_errors.push(format!(
                "database scan failed: exit {}: {}",
                output.exit_code,
                output.stderr.trim()
            ));
        } else {
            for line in output.stdout.lines() {
                let rel = line.trim().strip_prefix(&code_prefix).unwrap_or("");
                if rel.is_empty() {
                    continue;
                }
                // 逐段白名单（与旧本地扫描一致：非 [A-Za-z0-9._-] 段拒绝并日志可见）
                if rel.split('/').all(is_shell_safe_path_component) {
                    files.push(rel.to_string());
                } else {
                    tracing::warn!(
                        "[APP] skip database sql with unsafe path segments (not [A-Za-z0-9._-]): {rel}"
                    );
                }
            }
        }

        let mut report = DatabaseSqlReport {
            executed: Vec::new(),
            failed: scan_errors,
        };
        for rel in files {
            // rel 已过 is_shell_safe_path_component 白名单（收集时过滤），
            // 单引号包裹下 shell 不可注入
            let cmd = format!(
                "psql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" --set ON_ERROR_STOP=on -f '{code_root}/{}'",
                rel
            );
            let r = self
                .runtime
                .exec(app_id, vec!["sh".to_string(), "-c".to_string(), cmd])
                .await;
            match r {
                Ok(r) if r.exit_code == 0 => report.executed.push(rel),
                Ok(r) => {
                    report
                        .failed
                        .push(format!("{rel}: exit {}: {}", r.exit_code, r.stderr.trim()))
                }
                Err(e) => report.failed.push(format!("{rel}: exec failed: {e}")),
            }
        }
        Ok(report)
    }
}

/// 路径段白名单：`[A-Za-z0-9._-]`（防 zip 内恶意文件名破坏 `sh -c` 命令行——
/// 引号/空格/`$`/反斜杠等直接拒绝并日志可见，不做转义容错）。
fn is_shell_safe_path_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_safe_path_component_whitelist() {
        assert!(is_shell_safe_path_component("001_init.sql"));
        assert!(is_shell_safe_path_component("backend-go"));
        assert!(is_shell_safe_path_component("V1__up.down.sql"));
        // 引号/空格/$/反斜杠/中文 → 拒绝（恶意文件名防注入）
        assert!(!is_shell_safe_path_component("it's.sql"));
        assert!(!is_shell_safe_path_component("a b.sql"));
        assert!(!is_shell_safe_path_component("$(id).sql"));
        assert!(!is_shell_safe_path_component("back\\slash.sql"));
        assert!(!is_shell_safe_path_component("建表.sql"));
        assert!(!is_shell_safe_path_component(""));
        assert!(!is_shell_safe_path_component("."));
        assert!(!is_shell_safe_path_component(".."));
    }
}
