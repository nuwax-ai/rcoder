//! Docker UserApp 运行容器的 prod 挂载组装（从 docker_app_runtime.rs 拆出）。
//!
//! prod 四目录压平挂载（与 dev builder 完全同构）：宿主
//! `{userapp 根}/prod/{user_id}/` 下四目录（`{app_id}`、`data/{app_id}`、
//! `logs/{app_id}`、`agent-store/{app_id}`；布局单一事实源
//! [`shared_types::paths::userapp_prod_subpaths`]）→ 容器内
//! `/home/user/{app_id}`（workspace=发布代码根）、`/home/user/data`、
//! `/home/user/logs`、`/home/user/.agent-store`——app 间数据完全隔离。
//! 锚点反解失败 fail fast：静默降级等于 PG/制品落容器 overlay，容器删除即丢
//! （与 builder 卷同款 fail-fast 语义）。

use bollard::models::{Mount, MountType};
use container_runtime_api::ContainerRuntimeError;

/// 组装 prod 四目录 bind 挂载（含宿主目录预创建）。
///
/// 预创建走 rcoder 容器内锚点路径 mkdir（经 compose bind 双向同步宿主，daemon
/// 即刻可见 bind 源）；失败仅 warn 降级（Docker daemon 对缺失 bind 源会自动
/// mkdir 兜底，root 属主，风险低）。
pub(super) async fn build_prod_flat_mounts(
    app_id: &str,
    user_id: Option<&str>,
) -> container_runtime_api::ContainerRuntimeResult<Vec<Mount>> {
    let uid = user_id.unwrap_or(app_id);
    let host_root = crate::path::resolve_container_path_to_host(std::path::Path::new(
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
    ))
    .await
    .map_err(|e| {
        ContainerRuntimeError::DockerError(format!(
            "UserApp prod volume host path resolve failed (rcoder 容器需挂载 userapp-workspace 锚点): {e}"
        ))
    })?;
    let subs = shared_types::paths::userapp_prod_subpaths(uid, app_id);
    let mut mounts = Vec::with_capacity(4);
    for (rel, target) in subs.iter().zip(prod_flat_container_paths(app_id)) {
        let precreate =
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join(rel);
        if let Err(e) = tokio::fs::create_dir_all(&precreate).await {
            tracing::warn!(
                "[DOCKER_APP] create prod dir {} failed (continuing): {e}",
                precreate.display()
            );
        }
        let host_path = host_root.join(rel);
        tracing::info!(
            "[DOCKER_APP] prod flat mount: {} -> {target}",
            host_path.display()
        );
        mounts.push(Mount {
            target: Some(target),
            source: Some(host_path.to_string_lossy().to_string()),
            typ: Some(MountType::BIND),
            ..Default::default()
        });
    }
    Ok(mounts)
}

/// prod 压平挂载的容器内路径四元组（与 [`shared_types::paths::userapp_prod_subpaths`]
/// 段序一一配对）：workspace=`/home/user/{app_id}`（发布代码根）、
/// data/logs/agent-store 三常量——与 dev builder 容器内布局完全同图。
pub(crate) fn prod_flat_container_paths(app_id: &str) -> [String; 4] {
    [
        format!("{}/{}", shared_types::paths::USERAPP_DEV_HOME, app_id),
        shared_types::paths::USERAPP_DEV_DATA.to_string(),
        shared_types::paths::USERAPP_DEV_LOGS.to_string(),
        shared_types::paths::USERAPP_DEV_AGENT_STORE.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prod_flat_mounts_pair_with_layout_source() {
        let subs = shared_types::paths::userapp_prod_subpaths("u1", "a1");
        let targets = prod_flat_container_paths("a1");
        // 段序配对：workspace/data/logs/agent-store → /home/user 下四挂载点
        assert_eq!(
            targets,
            [
                "/home/user/a1".to_string(),
                "/home/user/data".to_string(),
                "/home/user/logs".to_string(),
                "/home/user/.agent-store".to_string(),
            ]
        );
        assert_eq!(subs[0], "prod/u1/a1");
        assert!(subs[1].ends_with("data/a1") && targets[1].ends_with("/data"));
        assert!(subs[2].ends_with("logs/a1") && targets[2].ends_with("/logs"));
        assert!(subs[3].ends_with("agent-store/a1") && targets[3].ends_with("/.agent-store"));
        // data 段兼容视图与布局事实源一致（清理链存量调用方依赖）
        assert_eq!(
            subs[1],
            shared_types::paths::userapp_prod_data_subpath("u1", "a1")
        );
    }
}
