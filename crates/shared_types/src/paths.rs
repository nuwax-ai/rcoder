//! 工作区路径常量 (单一事实源, 所有 crate 共用)。
//!
//! 统一管理容器内的工作区根目录, 避免 `/app/project_workspace`、
//! `/app/computer-project-workspace` 散落在 rcoder / docker_manager / agent_runner
//! 各自重复定义。路径拼接逻辑 (含 isolation_type 分支) 见
//! `rcoder::handler::utils::paths::{build_workspace_path, build_computer_workspace_path}`。

/// web agent 工作区根目录。
///
/// rcoder 主容器与 web agent sub-container 都挂此路径 (共享 PVC, subPath=workspace)。
///
/// 目录结构 (isolation=project, 默认):
/// ```text
/// /app/project_workspace/{project_id}/
/// ```
///
/// 目录结构 (isolation=tenant/space, 共享容器):
/// ```text
/// /app/project_workspace/{tenant_id}/{space_id}/{project_id}/
/// ```
pub const WORKSPACE_ROOT: &str = "/app/project_workspace";

/// computer agent 工作区根目录 (**rcoder 主容器视角**)。
///
/// 注意: computer agent sub-container 的挂载点是 `/home/user`
/// (= `{COMPUTER_WORKSPACE_ROOT}/{user_id}`), 不是此根路径本身。
/// rcoder 主容器通过此根访问所有 user 子目录 (挂载共享 PVC 根)。
///
/// 目录结构 (per-user, 与 subPath=user_id 挂载边界对齐):
/// ```text
/// /app/computer-project-workspace/{user_id}/
/// ```
///
/// 目录结构 (per-project):
/// ```text
/// /app/computer-project-workspace/{user_id}/{project_id}/
/// ```
pub const COMPUTER_WORKSPACE_ROOT: &str = "/app/computer-project-workspace";

/// UserApp 开发卷根目录 (**沙箱容器视角**)。
///
/// 独立共享卷: 沙箱 (ComputerAgentRunner) 挂载点为 `/home/user/userapp-workspace`
/// (K8s=共享 PVC `{ns}-rcoder-userapp-workspace`, Docker=bind `./userapp-workspace`);
/// builder 容器挂同一块卷到 `/app/userapp-workspace` (env 覆盖本默认值)。
/// file-server 全部 userapp 域接口 (文件操作镜像族 + build/detect/confirm/static)
/// 定位统一为 `{USERAPP_WORKSPACE_DIR}/{app_id}`——容器无关, 拓扑无关。
///
/// 目录结构:
/// ```text
/// /home/user/userapp-workspace/{app_id}/
/// ```
pub const USERAPP_WORKSPACE_ROOT: &str = "/home/user/userapp-workspace";

/// UserApp 开发卷根目录 (**rcoder 主容器视角**)。
///
/// rcoder 挂共享卷到此固定路径（helm deployment / docker-compose 均按此挂载），
/// 供 app_manager purge/destroy 时清理 `{root}/{app_id}/`（开发源码 + 构建制品 zip）。
pub const RCODER_USERAPP_WORKSPACE_ROOT: &str = "/app/userapp-workspace";

// ── UserApp 开发容器（UserAppBuilder）挂载压平契约 ─────────────────────────────
//
// 宿主机/K8s 卷内是完整树（compose: `{根}/dev/{user_id}/{app_id}/ + data/{app_id}/
// + logs/{app_id}/`；K8s: per-app PVC 卷内 `{app_id}/ + data/ + logs/`），
// 挂载把 env/user 层吸收——容器内只看到自己的三个目录，压平为：

/// 开发容器内 workspace 父目录（`USERAPP_WORKSPACE_DIR` env 值；
/// workspace = `{USERAPP_DEV_HOME}/{app_id}`，file-server 定位不变仍按 app_id）。
pub const USERAPP_DEV_HOME: &str = "/home/user";

/// 开发容器内持久数据目录（PG/dbx；`PGDATA`/`DBX_DATA_DIR` env 值的父目录）。
pub const USERAPP_DEV_DATA: &str = "/home/user/data";

/// 开发容器内持久日志目录（`USERAPP_LOG_DIR` env 值，TS file-server 同名约定）。
pub const USERAPP_DEV_LOGS: &str = "/home/user/logs";

/// 开发容器内 agent-store 实体存储目录（file-server `agent_store.rs` 契约
/// `user_root/.agent-store`——userapp 场景 user_root=/home/user；skills 经
/// 相对软链进 workspace `{app_id}/` 使用，挂载点布局与同一棵树路径一致）。
pub const USERAPP_DEV_AGENT_STORE: &str = "/home/user/.agent-store";

/// 开发容器内 PG 数据目录（`PGDATA` env 注入值）。
pub const USERAPP_DEV_PGDATA: &str = "/home/user/data/pg";

/// 开发容器内 dbx 数据目录（`DBX_DATA_DIR` env 注入值）。
pub const USERAPP_DEV_DBX_DATA: &str = "/home/user/data/dbx";

/// UserApp 运行容器内的应用代码根（**部署契约**：activate 后整体包落此目录，
/// app-runtime 镜像挂载点；database 目录 SQL 执行等容器内路径拼接收口于此）。
/// ```text
/// /app/code/                        ← workspace 整体包解包根
/// /app/code/database/*.sql          ← 平台自动执行的根级 SQL
/// /app/code/{子项目}/database/*.sql ← 子项目级 SQL
/// ```
pub const APP_CODE_ROOT: &str = "/app/code";
