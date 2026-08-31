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

/// Userapp 开发卷根目录 (**沙箱容器视角**)。
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

/// Userapp 开发卷根目录 (**rcoder 主容器视角**)。
///
/// rcoder 挂共享卷到此固定路径（helm deployment / docker-compose 均按此挂载），
/// 供 app_manager purge/destroy 时清理 `{root}/{app_id}/`（开发源码 + 构建制品 zip）。
pub const RCODER_USERAPP_WORKSPACE_ROOT: &str = "/app/userapp-workspace";

// ── Userapp 容器（dev builder / prod 运行容器）挂载压平契约 ────────────────────
//
// dev 与 prod 容器内布局完全同构（workspace/data/logs/agent-store 四目录压平在
// /home/user 下）；宿主机/K8s 卷内是完整树，挂载把 env/user 层吸收：
// - dev:  compose `{根}/dev/{user_id}/` 下 `{app_id}/ + data/{app_id}/ +
//   logs/{app_id}/ + agent-store/{app_id}/`；K8s per-app PVC 卷内四目录平级。
// - prod: compose `{根}/prod/{user_id}/` 下同构四目录；K8s 单块 per-app RBD PVC
//   卷内四目录平级（四 subPath 挂载）。
// 以下容器内路径常量为 dev/prod 两形态共用（值一致）。

/// Userapp 容器内 workspace 父目录（`USERAPP_WORKSPACE_DIR` env 值；
/// workspace = `{USERAPP_DEV_HOME}/{app_id}`，file-server 定位不变仍按 app_id）。
pub const USERAPP_DEV_HOME: &str = "/home/user";

/// Userapp 容器内持久数据目录（PG/dbx；`PGDATA`/`DBX_DATA_DIR` env 值的父目录）。
pub const USERAPP_DEV_DATA: &str = "/home/user/data";

/// Userapp 容器内持久日志目录（`USERAPP_LOG_DIR` env 值，TS file-server 同名约定）。
pub const USERAPP_DEV_LOGS: &str = "/home/user/logs";

/// Userapp 容器内 agent-store 实体存储目录（file-server `agent_store.rs` 契约
/// `user_root/.agent-store`——userapp 场景 user_root=/home/user；skills 经
/// 相对软链进 workspace `{app_id}/` 使用，挂载点布局与同一棵树路径一致——
/// workspace 与本目录同父（/home/user）是软链可解析的前提）。
pub const USERAPP_DEV_AGENT_STORE: &str = "/home/user/.agent-store";

/// Userapp 容器内 PG 数据目录（`PGDATA` env 注入值）。
pub const USERAPP_DEV_PGDATA: &str = "/home/user/data/pg";

/// Userapp 容器内 dbx 数据目录（`DBX_DATA_DIR` env 注入值）。
pub const USERAPP_DEV_DBX_DATA: &str = "/home/user/data/dbx";

// ── 挂载压平布局子路径（宿主树定位的单一事实源）──────────────────────────────
// compose mounts（dev builder 四 bind / prod 运行容器四 bind 组装）、
// dev cleanup（purge 通配清理）、docker_app_runtime/k8s_app_create/app_manager
// （prod 挂载与清理定位）共用——改布局只动这里。

/// Userapp 宿主树 `{dev|prod}/{user_id}/` 下四目录的 app 侧后缀段
/// （`{app_id}` / `data/{app_id}` / `logs/{app_id}` / `agent-store/{app_id}`）——
/// dev 与 prod 布局同构，共用此 suffix。
pub fn userapp_dev_app_suffixes(app_id: &str) -> [String; 4] {
    [
        app_id.to_string(),
        format!("data/{app_id}"),
        format!("logs/{app_id}"),
        format!("agent-store/{app_id}"),
    ]
}

/// Userapp 开发卷宿主树四目录的完整子路径（`dev/{user_id}/…`，锚点相对）。
pub fn userapp_dev_subpaths(user_id: &str, app_id: &str) -> [String; 4] {
    userapp_dev_app_suffixes(app_id).map(|s| format!("dev/{user_id}/{s}"))
}

/// Userapp prod 宿主树四目录的完整子路径（`prod/{user_id}/…`，锚点相对）——
/// 运行容器四 bind/四 subPath 挂载源与 clear/destroy 清理定位共用，
/// 与 [`userapp_dev_subpaths`] 布局同构（仅 dev→prod 一层之差）。
pub fn userapp_prod_subpaths(user_id: &str, app_id: &str) -> [String; 4] {
    userapp_dev_app_suffixes(app_id).map(|s| format!("prod/{user_id}/{s}"))
}

/// Userapp prod 数据目录子路径（`prod/{user_id}/data/{app_id}`，锚点相对）——
/// [`userapp_prod_subpaths`] 第二段的兼容视图（存量调用方零改动）。
pub fn userapp_prod_data_subpath(user_id: &str, app_id: &str) -> String {
    userapp_prod_subpaths(user_id, app_id)[1].clone()
}

/// Userapp 运行容器内的应用代码根（**部署契约**：activate 后整体包落此目录，
/// workspace 压平挂载点 `/home/user/{app_id}` 之下；database 目录 SQL 执行等
/// 容器内路径拼接收口于此）。
/// ```text
/// /home/user/{app_id}/code/                        ← workspace 整体包解包根
/// /home/user/{app_id}/code/database/*.sql          ← 平台自动执行的根级 SQL
/// /home/user/{app_id}/code/{子项目}/database/*.sql ← 子项目级 SQL
/// ```
pub fn app_code_root(app_id: &str) -> String {
    format!("{USERAPP_DEV_HOME}/{app_id}/code")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userapp_prod_subpaths_mirror_dev_layout() {
        let dev = userapp_dev_subpaths("u1", "a1");
        let prod = userapp_prod_subpaths("u1", "a1");
        assert_eq!(
            prod,
            [
                "prod/u1/a1".to_string(),
                "prod/u1/data/a1".to_string(),
                "prod/u1/logs/a1".to_string(),
                "prod/u1/agent-store/a1".to_string(),
            ]
        );
        // 布局同构：仅 dev→prod 前缀之差
        for (d, p) in dev.iter().zip(prod.iter()) {
            assert_eq!(p.replace("prod/", "dev/"), *d);
        }
    }

    #[test]
    fn userapp_prod_data_subpath_stays_compatible() {
        assert_eq!(
            userapp_prod_data_subpath("u1", "a1"),
            "prod/u1/data/a1",
            "存量调用方（storage/docker_app_runtime）依赖的 data 段视图不变"
        );
    }

    #[test]
    fn app_code_root_is_flattened_workspace_child() {
        assert_eq!(app_code_root("a1"), "/home/user/a1/code");
        assert!(
            app_code_root("a1").starts_with(USERAPP_DEV_HOME),
            "代码根在 workspace 压平挂载点之下（/home/user），随卷持久化"
        );
    }
}
