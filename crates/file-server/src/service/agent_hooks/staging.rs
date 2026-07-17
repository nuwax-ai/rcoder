//! staging 预生成 + 应用 (对齐 nuwax `stageRuntimeHookArtifacts` /
//! `applyStagedRuntimeHookArtifacts` + `clearHookArtifacts`)。
//!
//! Codex/OpenCode 运行时产物先在 `.tmp/hook-staging-*` 预生成, 成功后再替换工作区,
//! 缩小半更新窗口。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tokio::fs;

use crate::error::AppResult;

use super::codex::transform_hooks_for_codex;
use super::io_util::{now_nanos, write_json_file_atomic};
use super::opencode::{
    OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY, OPENCODE_PLUGIN_DIR, OPENCODE_PLUGIN_ENTRY,
    install_opencode_hooks_plugin, install_opencode_platform_env_plugin,
};

/// staging 预生成产物 (对齐 nuwax stageRuntimeHookArtifacts)。
pub(super) struct StagedRuntime {
    pub staging_root: PathBuf,
    pub codex_staging_root: PathBuf,
    pub codex_hooks: Option<Map<String, Value>>,
    pub plugin_installed: bool,
    pub platform_env_plugin_installed: bool,
}

/// 在 `.tmp/hook-staging-*` 预生成 Codex hooks + OpenCode 插件, 成功后再应用到工作区。
pub(super) async fn stage_runtime_hook_artifacts(
    workspace: &Path,
    hooks_map: &Map<String, Value>,
    install_platform_env: bool,
) -> AppResult<StagedRuntime> {
    let staging_root = workspace.join(".tmp").join(format!(
        "hook-staging-{}-{}",
        now_nanos(),
        std::process::id()
    ));
    let codex_staging_root = staging_root.join("codex");
    let codex_hooks_dir = codex_staging_root.join("hooks");
    let opencode_plugins_staging = staging_root.join("opencode").join("plugins");

    fs::create_dir_all(&codex_hooks_dir).await?;
    let codex_hooks = transform_hooks_for_codex(hooks_map, &codex_hooks_dir).await?;
    let plugin_installed = install_opencode_hooks_plugin(&opencode_plugins_staging).await?;
    let platform_env_plugin_installed = if install_platform_env {
        install_opencode_platform_env_plugin(&opencode_plugins_staging).await?
    } else {
        false
    };

    Ok(StagedRuntime {
        staging_root,
        codex_staging_root,
        codex_hooks,
        plugin_installed,
        platform_env_plugin_installed,
    })
}

/// 将 staging 产物应用到工作区 (对齐 nuwax applyStagedRuntimeHookArtifacts)。
pub(super) async fn apply_staged_runtime_hook_artifacts(
    workspace: &Path,
    staged: &StagedRuntime,
) -> AppResult<()> {
    let codex_root = workspace.join(".codex");
    let codex_hooks_target = codex_root.join("hooks");
    let opencode_plugins_target = workspace.join(".opencode").join("plugins");

    clear_hook_artifacts(workspace).await?;
    fs::create_dir_all(&codex_root).await?;

    // Codex hooks 目录 (含 http wrapper 脚本)
    let staged_codex_hooks_dir = staged.codex_staging_root.join("hooks");
    if fs::try_exists(&staged_codex_hooks_dir)
        .await
        .unwrap_or(false)
    {
        let _ = fs::remove_dir_all(&codex_hooks_target).await;
        fs::rename(&staged_codex_hooks_dir, &codex_hooks_target).await?;
    }

    // .codex/hooks.json
    if let Some(codex_hooks) = &staged.codex_hooks {
        write_json_file_atomic(
            &codex_root.join("hooks.json"),
            &json!({ "hooks": codex_hooks }),
        )
        .await?;
        tracing::info!(
            events = ?codex_hooks.keys().collect::<Vec<_>>(),
            "Written .codex/hooks.json"
        );
    }

    // opencode-hooks-plugin
    if staged.plugin_installed {
        fs::create_dir_all(&opencode_plugins_target).await?;
        let staged_plugins = staged.staging_root.join("opencode").join("plugins");
        let staged_entry = staged_plugins.join(OPENCODE_PLUGIN_ENTRY);
        if fs::try_exists(&staged_entry).await.unwrap_or(false) {
            fs::copy(
                &staged_entry,
                opencode_plugins_target.join(OPENCODE_PLUGIN_ENTRY),
            )
            .await?;
        }
        let staged_plugin_dir = staged_plugins.join(OPENCODE_PLUGIN_DIR);
        if fs::try_exists(&staged_plugin_dir).await.unwrap_or(false) {
            let target_dir = opencode_plugins_target.join(OPENCODE_PLUGIN_DIR);
            let _ = fs::remove_dir_all(&target_dir).await;
            crate::service::fs_util::copy_dir_filtered(&staged_plugin_dir, &target_dir, &[], &[])
                .await?;
        }
    }

    // opencode-platform-env-plugin
    if staged.platform_env_plugin_installed {
        fs::create_dir_all(&opencode_plugins_target).await?;
        let staged_pe = staged
            .staging_root
            .join("opencode")
            .join("plugins")
            .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY);
        if fs::try_exists(&staged_pe).await.unwrap_or(false) {
            fs::copy(
                &staged_pe,
                opencode_plugins_target.join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY),
            )
            .await?;
        }
    }
    Ok(())
}

/// 清除 hook 相关产物 (不含 permissions 等其他 settings 字段), 对齐 nuwax clearHookArtifacts。
pub(super) async fn clear_hook_artifacts(workspace: &Path) -> AppResult<()> {
    let targets: [PathBuf; 6] = [
        workspace.join(".codex").join("hooks.json"),
        workspace.join(".codex").join("hooks"),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLUGIN_ENTRY),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLUGIN_DIR),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY),
        workspace.join(".claude").join("hooks"),
    ];
    for target in targets {
        remove_path_best_effort(&target).await;
    }
    Ok(())
}

/// 删除文件或目录, NotFound 不计错 (对齐 nuwax fs.rm { recursive, force })。
async fn remove_path_best_effort(path: &Path) {
    let meta = match fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let r = if meta.is_dir() {
        fs::remove_dir_all(path).await
    } else {
        fs::remove_file(path).await
    };
    if let Err(e) = r
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "remove hook artifact failed");
    }
}
