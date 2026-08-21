//! UserApp 容器内 PostgreSQL 管理（从 service.rs 拆出，extension-impl）。
//!
//! reset-password / create-database（exec psql）+ align_db_credentials（凭据对齐，
//! 流程单头 `shared_types::align_pg_credentials`）+ exec_psql / database_exists / ensure_app_running。

use async_trait::async_trait;
use tracing::info;

use super::models::*;
use super::service::AppService;
use super::utils::*;

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
        // SQL 字符串里 ' 转义为 ''(防注入)。ON_ERROR_STOP=1:出错 exit≠0。
        let safe_pw = req.new_password.replace('\'', "''");
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c "ALTER USER \"$POSTGRES_USER\" WITH PASSWORD '{safe_pw}'""#,
            ),
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
        // CREATE DATABASE "{db}"[ OWNER "{owner}"] —— 双引号 PG 标识符," 转义为 ""
        let safe_db = req.database.replace('"', "\"\"");
        let owner_clause = req
            .owner
            .as_ref()
            .map(|o| format!(" OWNER \"{}\"", o.replace('"', "\"\"")))
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
        self.ensure_app_running(app_id).await?;
        let runner = RuntimeExecRunner {
            service: self,
            app_id,
        };
        let outcome = shared_types::align_pg_credentials(&runner, &req.username, &req.password)
            .await
            .map_err(AppOperationError::Backend)?;
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
    /// 目录内按文件名升序。逐文件 exec 容器内 `psql -f`（容器内路径 `/app/code/{rel}`，
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

        // 收集 SQL（根 database 优先，子项目目录名序，目录内文件名升序）
        let mut files: Vec<String> = Vec::new();
        let root_db = code.join("database");
        if root_db.is_dir() {
            collect_sql_files(&root_db, "database", &mut files)?;
        }
        let mut subdirs: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&code).map_err(map_io_error_db)? {
            let entry = entry.map_err(map_io_error_db)?;
            if entry.file_type().map_err(map_io_error_db)?.is_dir()
                && entry.path().join("database").is_dir()
            {
                subdirs.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        subdirs.sort();
        for dir in subdirs {
            collect_sql_files(
                &code.join(&dir).join("database"),
                &format!("{dir}/database"),
                &mut files,
            )?;
        }

        let mut report = DatabaseSqlReport {
            executed: Vec::new(),
            failed: Vec::new(),
        };
        for rel in files {
            let cmd = format!(
                "psql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" --set ON_ERROR_STOP=on -f '/app/code/{rel}'"
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

/// 收集目录下 `*.sql` 文件为 `{prefix}/{name}`（文件名升序）。
fn collect_sql_files(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) -> AppResult<()> {
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(map_io_error_db)? {
        let entry = entry.map_err(map_io_error_db)?;
        let ft = entry.file_type().map_err(map_io_error_db)?;
        if ft.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sql") {
                names.push(name);
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
