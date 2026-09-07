//! Docker 侧 WorkspaceRuntime 实现（workspace 卷命名/枚举/销毁）与 dev 卷扫描。

use async_trait::async_trait;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, WorkspaceRuntime};
use shared_types::ServiceType;

use super::docker_runtime::DockerRuntime;

/// `list_workspace_identifiers` 仅实现 dev（UserappBuilder）形态（目录树扫描），
/// prod 维持 trait 默认空（孤儿检测依赖 list_deployments 兜底——存量缺口）。
/// **`destroy_app_pvc` 重写** (Docker 模式 destroy = 删 app workspace 目录, 对应 K8s 删 PVC+subvolume).
#[async_trait]
impl WorkspaceRuntime for DockerRuntime {
    async fn workspace_volume_name(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // Docker 持久卷 = userapp-workspace 树四目录（uid 维度经通配定位，
        // 无单一物理路径）——返回展示标识串（与 K8s 返回 PVC 名对称：标识而非
        // 可 stat 路径；存在性判定由调用方 storage 层用元数据 uid 精确定位）。
        // dev（UserappBuilder）与 prod（Userapp）仅树前缀一层之差。
        if app_id.is_empty() || app_id.contains('/') || app_id.contains('\\') {
            return Err(ContainerRuntimeError::DockerError(format!(
                "workspace_volume_name: invalid app_id {app_id:?}"
            )));
        }
        let stage = if *service_type == ServiceType::UserappBuilder {
            "dev"
        } else {
            "prod"
        };
        Ok(format!(
            "{}/{stage}/*/{}",
            shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
            app_id
        ))
    }

    /// Docker 无 PVC label 集——dev（UserappBuilder）形态按 dev 树目录扫描反解
    /// identifier：`dev/{uid}/` 一层 × 其下 app 目录（跳过同层的 data/logs/
    /// agent-store 三个兄弟目录，它们按 app_id 键不是 uid）。prod 形态维持
    /// 空（Docker 孤儿检测非关键，见 trait 注释）。
    async fn list_workspace_identifiers(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Vec<String>> {
        if *service_type != ServiceType::UserappBuilder {
            return Ok(vec![]);
        }
        let dev_root =
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join("dev");
        scan_dev_workspace_identifiers(&dev_root).await
    }

    async fn destroy_app_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        // Docker 无 PVC 概念；destroy = 删 userapp-workspace prod 树该 app 的四目录
        // （workspace + data/logs/agent-store，对应 K8s 删单卷 PVC）+ 兜底删旧
        // RCODER_WORKSPACE_ROOT/{app_id} 制品目录（四目录化前的旧布局孤儿）。
        // uid 不在本层（无元数据视图）→ 通配扫 `prod/*/` 一层按
        // userapp_prod_subpaths(uid, app_id) 精确匹配四段——与 dev cleanup
        // （dev_cleanup.rs 通配 dev/*/）完全同款模式，顺带覆盖 uid 兜底不一致的目录。
        // 幂等：目录不存在返回 Ok（对应 K8s PVC 404→Ok）。app_id 经 service 层
        // validate_app_id 校验（DNS-1123，无 .. / 路径穿越），join 安全。
        if app_id.is_empty() || app_id.contains('/') || app_id.contains('\\') {
            return Err(ContainerRuntimeError::DockerError(format!(
                "destroy_app_pvc: invalid app_id {app_id:?}"
            )));
        }
        let prod_root =
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join("prod");
        // 全段走 tokio::fs：本函数是 async fn 且下方删除已是 remove_dir_all().await，
        // 混用阻塞 std::fs 会占住 tokio worker 线程。错误处理对齐同文件
        // scan_dev_workspace_identifiers —— 上抛而非静默跳过：漏扫某个 uid 目录会让
        // 该 app 的 prod 目录悄悄逃过销毁，而本函数的语义是"删干净"。
        if tokio::fs::metadata(&prod_root)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let mut rd = tokio::fs::read_dir(&prod_root).await.map_err(|e| {
                ContainerRuntimeError::DockerError(format!(
                    "destroy_app_pvc: read_dir {}: {e}",
                    prod_root.display()
                ))
            })?;
            let mut uid_entries = Vec::new();
            loop {
                let entry = match rd.next_entry().await {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(e) => {
                        return Err(ContainerRuntimeError::DockerError(format!(
                            "destroy_app_pvc: iterate {}: {e}",
                            prod_root.display()
                        )));
                    }
                };
                // file_type() 走 readdir 的 d_type，不额外 stat。
                //
                // 注意这里与旧实现有一处**有意的语义差异**：旧代码是
                // `.filter(|e| e.path().is_dir())`，`Path::is_dir()` 跟随符号链接；
                // `DirEntry::file_type()` 不跟随（d_type / symlink_metadata）。
                // 对这条删除路径而言不跟随更安全：若 `prod/{uid}` 是指向别处的符号链接，
                // 跟随它会拿链接名拼出 userapp_prod_subpaths 再 remove_dir_all，
                // 等于删到 prod 树之外。宁可漏扫一个符号链接 uid，也不越界删除。
                // （同文件 scan_dev_workspace_identifiers 是**列举**语义，漏项会导致
                // orphan 误判，那边刻意保留跟随符号链接——两者风险方向相反。）
                if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    uid_entries.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            for uid in uid_entries {
                for sub in shared_types::paths::userapp_prod_subpaths(&uid, app_id) {
                    let dir =
                        std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
                            .join(&sub);
                    if tokio::fs::try_exists(&dir).await.map_err(|e| {
                        ContainerRuntimeError::DockerError(format!(
                            "destroy_app_pvc: stat {}: {e}",
                            dir.display()
                        ))
                    })? {
                        tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
                            ContainerRuntimeError::DockerError(format!(
                                "destroy_app_pvc: remove {}: {}",
                                dir.display(),
                                e
                            ))
                        })?;
                        tracing::info!("[Docker] app prod dir destroyed: {}", dir.display());
                    }
                }
            }
        }
        // 旧布局兜底：RCODER_WORKSPACE_ROOT/{app_id}（默认 /app/project_workspace/apps，
        // 与 AppManagerConfig::get_workspace_root 同源）——存量升级后制品目录孤儿。
        let ws_root = std::env::var("RCODER_WORKSPACE_ROOT")
            .unwrap_or_else(|_| "/app/project_workspace/apps".to_string());
        let legacy_dir = std::path::Path::new(&ws_root).join(app_id);
        if tokio::fs::try_exists(&legacy_dir).await.map_err(|e| {
            ContainerRuntimeError::DockerError(format!(
                "destroy_app_pvc: stat legacy {}: {e}",
                legacy_dir.display()
            ))
        })? {
            tokio::fs::remove_dir_all(&legacy_dir).await.map_err(|e| {
                ContainerRuntimeError::DockerError(format!(
                    "destroy_app_pvc: remove legacy {}: {}",
                    legacy_dir.display(),
                    e
                ))
            })?;
            tracing::info!(
                "[Docker] legacy app workspace destroyed: {}",
                legacy_dir.display()
            );
        }
        Ok(())
    }
}

/// dev 树 identifier 扫描（`list_workspace_identifiers(UserappBuilder)` 的实现体，
/// 提为自由函数便于单测）：`dev/{uid}/` 一层 × 其下 app 目录；跳过同层按 app_id
/// 键的 data/logs/agent-store 兄弟目录；dev 树不存在 = 空（幂等）。
pub(super) async fn scan_dev_workspace_identifiers(
    dev_root: &std::path::Path,
) -> ContainerRuntimeResult<Vec<String>> {
    let mut uid_entries = match tokio::fs::read_dir(dev_root).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => {
            return Err(ContainerRuntimeError::DockerError(format!(
                "list dev workspace identifiers: read {}: {e}",
                dev_root.display()
            )));
        }
    };
    let mut ids = std::collections::BTreeSet::new();
    loop {
        // 错误上抛（对齐顶层 read_dir 的处理）：静默截断会让漏扫用户的
        // workspace 逃过对账 —— orphan 误判/清毁前置校验误放行都源于此
        let uid_dir = match uid_entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "list dev workspace identifiers: iterate {}: {e}",
                    dev_root.display()
                )));
            }
        };
        let uid_path = uid_dir.path();
        // tokio::fs::metadata 而非阻塞的 Path::is_dir()（本函数是 async fn，跑在 tokio
        // worker 上）。刻意不用 DirEntry::file_type()：它不跟随符号链接，而本函数是
        // **列举**语义，漏掉一个 uid 会让该用户的 workspace 逃过对账（与本函数上方
        // "错误上抛而非静默截断"同理）。metadata() 与 Path::is_dir() 同样跟随符号链接，
        // 语义不变；出错按"不是目录"跳过，对齐 Path::is_dir() 遇错返回 false。
        if !tokio::fs::metadata(&uid_path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let mut app_entries = match tokio::fs::read_dir(&uid_path).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "list dev workspace identifiers: read {}: {e}",
                    uid_path.display()
                )));
            }
        };
        loop {
            let app_dir = match app_entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    return Err(ContainerRuntimeError::DockerError(format!(
                        "list dev workspace identifiers: iterate {}: {e}",
                        uid_path.display()
                    )));
                }
            };
            let name = app_dir.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), "data" | "logs" | "agent-store") {
                continue;
            }
            // 同上：tokio::fs::metadata 替代阻塞的 Path::is_dir()，跟随符号链接语义不变
            if tokio::fs::metadata(app_dir.path())
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                ids.insert(name);
            }
        }
    }
    Ok(ids.into_iter().collect())
}
