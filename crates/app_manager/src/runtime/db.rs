//! UserApp 容器内 PostgreSQL 管理（从 service.rs 拆出，extension-impl）。
//!
//! reset-password / create-database（exec psql）+ align_db_credentials（凭据对齐，
//! 流程单头 `shared_types::align_pg_credentials`）+ exec_psql / database_exists / ensure_app_running。

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
    /// 重置 app 容器内 PG 密码(rcoder exec 容器内 psql ALTER USER,本地 trust 认证绕过当前密码)。
    /// 解决"用户忘记密码进不去 pgweb"的死锁(pgweb 要当前密码,rcoder 用容器内 trust 免密)。
    pub async fn reset_db_password(
        &self,
        app_id: &str,
        req: ResetDbPasswordRequest,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        if req.new_password.is_empty() {
            return Err(AppOperationError::Validation(
                "new_password must not be empty".to_string(),
            ));
        }
        self.ensure_app_running(app_id).await?;
        // 容器内 sh 展开 $POSTGRES_USER(镜像 ENV,create 时用户 env 覆盖);rcoder 无状态不知值。
        // psql -U $POSTGRES_USER 本地 trust 认证(start-app.sh initdb --auth-local=trust)免密。
        // 重置目标用 SQL 的 CURRENT_USER(= 连接用户), 消除 shell 双引号变量展开依赖。
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            shared_types::pg_utils::pg_alter_current_user_password_cmd(&req.new_password),
        ];
        self.exec_psql(
            app_id,
            cmd,
            &format!("[APP] reset_db_password failed app_id={app_id}"),
        )
        .await?;
        info!("[APP] PG password reset: {}", app_id);
        Ok(())
    }

    /// 新建 PG 库(rcoder exec 容器内 psql CREATE DATABASE)。API 化建库(Java/CI 自动化)。
    pub async fn create_database(&self, app_id: &str, req: CreateDatabaseRequest) -> AppResult<()> {
        validate_app_id(app_id)?;
        validate_pg_identifier(&req.database)?;
        if let Some(owner) = &req.owner {
            validate_pg_identifier(owner)?;
        }
        self.ensure_app_running(app_id).await?;
        // 先查是否已存在(check-then-act): PG 不支持 CREATE DATABASE IF NOT EXISTS、也不能进事务/DO 块。
        // 故先 SELECT pg_database 判定, 避免 CREATE 失败后靠 stderr 文本(随 PG 版本/locale 变)判"已存在"。
        if self.database_exists(app_id, &req.database).await? {
            return Err(AppOperationError::AlreadyExists(format!(
                "database {} already exists",
                req.database
            )));
        }
        // CREATE DATABASE "{db}"[ OWNER "{owner}"] —— 标识符引经 pg_utils 单头
        // （validate 白名单本就拒绝 "，此处转义为纵深防御）
        let safe_db = shared_types::pg_utils::pg_quote_ident(&req.database)
            .trim_matches('"')
            .to_string();
        let owner_clause = req
            .owner
            .as_ref()
            .map(|o| format!(" OWNER {}", shared_types::pg_utils::pg_quote_ident(o)))
            .unwrap_or_default();
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c 'CREATE DATABASE "{safe_db}"{owner_clause}'"#,
            ),
        ];
        let ctx = format!("[APP] create_database failed app_id={app_id}");
        let r = self
            .runtime
            .exec(app_id, cmd)
            .await
            .map_err(|e| map_runtime_error(&ctx, e))?;
        if r.exit_code != 0 {
            // 罕见竞态: SELECT 时不存在、CREATE 时被并发创建 → 再查一次精确判定, 仍不靠 stderr 文本。
            if self.database_exists(app_id, &req.database).await? {
                return Err(AppOperationError::AlreadyExists(format!(
                    "database {} already exists",
                    req.database
                )));
            }
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        info!(
            "[APP] database created: {} (app_id={})",
            req.database, app_id
        );
        Ok(())
    }

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

    /// exec 容器内 psql 命令，exit_code != 0 → Backend 错误（含 stderr 摘要）。
    ///
    /// reset_db_password 共用。create_database 因需区分"库已存在"(AlreadyExists) 不复用此函数。
    async fn exec_psql(&self, app_id: &str, command: Vec<String>, ctx: &str) -> AppResult<()> {
        let r = self
            .runtime
            .exec(app_id, command)
            .await
            .map_err(|e| map_runtime_error(ctx, e))?;
        if r.exit_code != 0 {
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        Ok(())
    }

    /// 查询 app 容器 PG 里某库是否已存在（psql `-tAc SELECT pg_database`）。
    /// `-tAc` 取无表头纯输出: 命中输出 `1`、未命中输出空 → 比 CREATE 失败后解析 stderr 稳定。
    /// `db` 已过 `validate_pg_identifier` 白名单(`[a-zA-Z0-9_]`), 安全内联到字符串字面量。
    /// create_database 用此做 check-then-act(替代旧版靠 stderr 文本判"已存在")。
    async fn database_exists(&self, app_id: &str, db: &str) -> AppResult<bool> {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='{db}'""#,
            ),
        ];
        let ctx = format!("[APP] check database exists failed app_id={app_id}");
        let r = self
            .runtime
            .exec(app_id, cmd)
            .await
            .map_err(|e| map_runtime_error(&ctx, e))?;
        if r.exit_code != 0 {
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        Ok(r.stdout.trim() == "1")
    }

    /// 校验 app 处于 Running 阶段（exec psql 的前置条件）。
    ///
    /// Stopped/Starting 等给 InvalidState 友好错误而非让 exec 失败（exec 在 Stopped 时
    /// 报容器不存在的 Backend 错误，对用户不友好）。reset_db_password / create_database 共用。
    async fn ensure_app_running(&self, app_id: &str) -> AppResult<()> {
        // validate_app_id 由调用方（reset_db_password/create_database）先做（Fail Fast：参数校验
        // 在 K8s API 调用前，非法 app_id 不浪费 RTT）。此方法仅做 phase 检查（单一职责）。
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        if status.phase != "Running" {
            return Err(AppOperationError::InvalidState(format!(
                "app {app_id} not running (phase={}), exec psql requires a live container",
                status.phase
            )));
        }
        Ok(())
    }

    /// database 目录 SQL 自动执行（发布 activate 后调用；失败仅收集不阻断发布）。
    ///
    /// 扫描 code 目录（rcoder 视角 PVC 路径）下的 `database/*.sql`：
    /// 1. code 根 `database/`（建库/扩展类，先执行）
    /// 2. 各一级子项目 `{dir}/database/`（目录名排序）
    ///
    /// 目录内按文件名升序。逐文件 exec 容器内 `psql -f`（容器内路径
    /// `{APP_CODE_ROOT}/{rel}`，
    /// `ON_ERROR_STOP=on` 单文件原子性），单文件失败收集进 report 继续下一文件。
    pub async fn execute_database_sql(&self, app_id: &str) -> AppResult<DatabaseSqlReport> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let code = app_dir.join("code");
        if !code.is_dir() {
            return Ok(DatabaseSqlReport {
                executed: vec![],
                failed: vec![],
            });
        }

        // 同步 fs 扫描放 spawn_blocking：对象是 per-app PVC（CephFS 网络卷），
        // 挂载抖动时 read_dir/file_type/is_dir 会阻塞 tokio worker
        // （release_store 已有同款先例）。
        let mut files: Vec<String> = Vec::new();
        let mut scan_errors: Vec<String> = Vec::new();
        let code_for_task = code.clone();
        let (root_files, root_err) = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            match collect_sql_files(&code_for_task.join("database"), "database", &mut out) {
                Ok(()) => (out, None),
                Err(e) => (out, Some(format!("database: scan failed: {e}"))),
            }
        })
        .await
        .map_err(|e| AppOperationError::Backend(format!("scan task join: {e}")))?;
        files.extend(root_files);
        if let Some(e) = root_err {
            // "失败不阻断"契约对扫描同样成立：根目录扫描失败收集进 report 继续
            scan_errors.push(e);
        }
        // 子项目 database 目录发现（同步 fs 同样入 blocking；每目录扫描失败收集进 report 继续）
        let code_for_scan = code.clone();
        let subdirs: Vec<String> = tokio::task::spawn_blocking(move || {
            let mut subdirs = Vec::new();
            let entries = match std::fs::read_dir(&code_for_scan) {
                Ok(rd) => rd,
                Err(_) => return subdirs,
            };
            for entry in entries.flatten() {
                if entry
                    .file_type()
                    .is_ok_and(|ft| ft.is_dir())
                    && entry.path().join("database").is_dir()
                {
                    let dir = entry.file_name().to_string_lossy().to_string();
                    if is_shell_safe_path_component(&dir) {
                        subdirs.push(dir);
                    } else {
                        tracing::warn!(
                            "[APP] skip database subdir with unsafe name (not [A-Za-z0-9._-]): {dir}"
                        );
                    }
                }
            }
            subdirs.sort();
            subdirs
        })
        .await
        .map_err(|e| AppOperationError::Backend(format!("scan task join: {e}")))?;
        for dir in subdirs {
            let dir_db = code.join(&dir).join("database");
            let prefix = format!("{dir}/database");
            let (dir_files, dir_err) = tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                match collect_sql_files(&dir_db, &prefix, &mut out) {
                    Ok(()) => (out, None),
                    Err(e) => (out, Some(format!("{prefix}: scan failed: {e}"))),
                }
            })
            .await
            .map_err(|e| AppOperationError::Backend(format!("scan task join: {e}")))?;
            files.extend(dir_files);
            if let Some(e) = dir_err {
                scan_errors.push(e);
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
                "psql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" --set ON_ERROR_STOP=on -f '{}/{}'",
                shared_types::paths::APP_CODE_ROOT,
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

/// 收集目录下 `*.sql` 文件为 `{prefix}/{name}`（文件名升序）。
/// 文件名须过 [`is_shell_safe_path_component`] 白名单——不过者跳过并日志可见。
fn collect_sql_files(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) -> AppResult<()> {
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(map_io_error_db)? {
        let entry = entry.map_err(map_io_error_db)?;
        let ft = entry.file_type().map_err(map_io_error_db)?;
        if ft.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sql") {
                if is_shell_safe_path_component(&name) {
                    names.push(name);
                } else {
                    tracing::warn!(
                        "[APP] skip database sql with unsafe filename (not [A-Za-z0-9._-]): {name}"
                    );
                }
            }
        }
    }
    names.sort();
    for name in names {
        out.push(format!("{prefix}/{name}"));
    }
    Ok(())
}

fn map_io_error_db(e: std::io::Error) -> AppOperationError {
    AppOperationError::Backend(format!("scan database dir: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_safe_path_component_whitelist() {
        assert!(is_shell_safe_path_component("001_init.sql"));
        assert!(is_shell_safe_path_component("backend-go"));
        assert!(is_shell_safe_path_component("V1__up.down.sql"));
        // 引号/空格/$/反斜杠/中文 → 拒绝（zip 内恶意文件名防注入）
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
