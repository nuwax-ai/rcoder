//! Agent 安装入口
//!
//! 把"平台匹配 → 下载到缓存 → 复制/解压到安装目录 → 更新 registry"串成一条
//! 通用流程，供 rcoder（主安装路径）与 agent_runner（bundle 缺失兜底自装）复用。

use std::path::Path;

use shared_types::version_util::normalize_platform_key;
use shared_types::{AppError, error_codes};
use tracing::{info, warn};

use crate::manager::{AgentDownloadManager, DownloadResult};
use crate::registry_update::update_registry;

/// 从 URL 安装 agent（通用核心逻辑）
///
/// # 流程
/// 1. 平台匹配：根据当前系统 OS/ARCH 从 platforms 中获取对应 URL
/// 2. 缓存检查：已缓存则跳过下载
/// 3. 下载到缓存目录
/// 4. 复制到安装目录（自动解压）
/// 5. 更新 registry.json
///
/// # 参数
/// - `download_manager`：下载管理器（持有缓存目录）
/// - `install_dir`：安装根目录，会写入 `install_dir/{agent_id}/{version}/`
///
/// # 返回
/// `(DownloadResult, platform_key)` - 下载结果和匹配的平台 key
///
/// # Errors
/// 平台未找到 / 下载 / 复制 / registry 更新任一步失败均返回 `AppError`。
pub async fn install_agent(
    download_manager: &AgentDownloadManager,
    agent_id: &str,
    version: &str,
    command: &str,
    args: &[String],
    platforms: &std::collections::HashMap<String, shared_types::PlatformEntry>,
    install_dir: &Path,
) -> Result<(DownloadResult, String), AppError> {
    // 1. 平台匹配
    let sys_info = shared_types::SystemInfo::current();
    let platform_key = normalize_platform_key(&sys_info.os, &sys_info.arch);
    let platform_entry = platforms.get(&platform_key).ok_or_else(|| {
        AppError::with_message(
            error_codes::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND,
            format!(
                "platform not found: {} (available: {:?})",
                platform_key,
                platforms.keys().collect::<Vec<_>>()
            ),
        )
    })?;

    // 2. 缓存检查（仅用于日志区分）
    let from_cache = download_manager.is_cached_async(agent_id, version).await;

    // 3. 下载到缓存
    let download_result = download_manager
        .download_to_cache(agent_id, version, &platform_entry.url)
        .await
        .map_err(|e| {
            warn!(
                "📦 [INSTALL] Download failed: agent_id={}, version={}, error={}",
                agent_id, version, e
            );
            AppError::with_message(
                error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
                format!("download failed: {}", e),
            )
        })?;

    // 4. 复制到安装目录（自动解压）
    download_manager
        .copy_to_target(agent_id, version, install_dir)
        .await
        .map_err(|e| {
            warn!(
                "📦 [INSTALL] Copy failed: agent_id={}, version={}, error={}",
                agent_id, version, e
            );
            AppError::with_message(
                error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
                format!("copy failed: {}", e),
            )
        })?;

    // 5. 更新 registry
    update_registry(install_dir, agent_id, version, command, args)
        .await
        .map_err(|e| {
            warn!(
                "📦 [INSTALL] Registry update failed: agent_id={}, version={}, error={}",
                agent_id, version, e
            );
            AppError::with_message(
                error_codes::ERR_AGENT_MGMT_INSTALL_FAILED,
                format!("registry update failed: {}", e),
            )
        })?;

    if from_cache {
        info!(
            "📦 [INSTALL] Agent installed from cache: agent_id={}, version={}, platform={}",
            agent_id, version, platform_key
        );
    } else {
        info!(
            "📦 [INSTALL] Agent installed: agent_id={}, version={}, platform={}, file_size={}",
            agent_id, version, platform_key, download_result.file_size
        );
    }

    Ok((download_result, platform_key))
}

/// 判断 agent 是否已安装到指定 install_dir。
///
/// 判据（布局无关）：`install_dir/{agent_id}/{version}` 目录存在且非空。
/// 该路径与 [`AgentDownloadManager::copy_to_target`] 写入的目标同源，
/// 保证"判定路径 == 安装路径"。
///
/// 注意：不能用 `AgentDownloadManager::is_cached`——它只判下载缓存
/// `cache_dir/{agent_id}/{version}`（全局），与具体 install_dir（per-user）无关，
/// 会导致"首个 user 装上、其余 user 被跳过"的 bug。
pub fn is_agent_installed(install_dir: &Path, agent_id: &str, version: &str) -> bool {
    let target = install_dir.join(agent_id).join(version);
    std::fs::read_dir(&target)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_agent_installed_detects_state() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path();

        // 不存在 → 未安装
        assert!(!is_agent_installed(install_dir, "agentA", "1.0.0"));

        // 版本目录存在且有文件 → 已安装
        let ver_dir = install_dir.join("agentA").join("1.0.0");
        std::fs::create_dir_all(&ver_dir).unwrap();
        std::fs::write(ver_dir.join("bundle.mjs"), "x").unwrap();
        assert!(is_agent_installed(install_dir, "agentA", "1.0.0"));

        // 版本目录存在但为空（不完整安装）→ 未安装
        let empty_ver = install_dir.join("agentB").join("2.0.0");
        std::fs::create_dir_all(&empty_ver).unwrap();
        assert!(!is_agent_installed(install_dir, "agentB", "2.0.0"));
    }

    /// 回归测试：per-user/per-target 安装状态独立判定（不能被全局缓存短路）。
    #[test]
    fn per_target_install_state_independent() {
        let user1 = tempfile::tempdir().unwrap();
        let user2 = tempfile::tempdir().unwrap();
        let agent_id = "33290548";
        let version = "1.0.1";

        // user-1 装好（版本目录非空）
        let u1_ver = user1.path().join(agent_id).join(version);
        std::fs::create_dir_all(&u1_ver).unwrap();
        std::fs::write(u1_ver.join("bundle.mjs"), "x").unwrap();

        // user-1 已装、user-2 未装 —— 两者独立判定
        assert!(is_agent_installed(user1.path(), agent_id, version));
        assert!(!is_agent_installed(user2.path(), agent_id, version));
    }
}
