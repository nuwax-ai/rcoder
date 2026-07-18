//! Agent hook 配置写入 (对齐 nuwax `utils/computer/hookConfigUtils.js`)。
//!
//! 写入 Claude Code / Codex / OpenCode 三套 Hook 相关配置:
//! - Claude Code: `.claude/settings.json` (hooks + permissions) + `.mcp.json` + `.claude/hooks/` 脚本
//! - Codex: `.codex/hooks.json` (http hook 转 command wrapper 脚本) + `.codex/hooks/*.sh`
//! - OpenCode: `.opencode/plugins/opencode-hooks-plugin` + 可选 platform-env 插件
//!
//! 设计要点 (与 nuwax 一致):
//! - 仅在对应配置解析成功时才清除并重写, 避免无效 payload 误删旧配置
//! - Codex/OpenCode 运行时产物先在 `.tmp/hook-staging-*` staging 目录预生成, 成功后再替换工作区,
//!   缩小半更新窗口
//! - 原子写 (staging tmp + rename) + 损坏 JSON 保护 (解析失败保留旧文件)
//! - hook 脚本路径校验防穿越与配置覆盖 (限定在 `.claude/hooks/` 下)
//!
//! hook 配置是 schemaless 的嵌套 JSON, 用 `serde_json::Value` 透传处理。
//!
//! 子模块: [`claude`] (Claude Code settings/scripts) / [`parse`] (hooks 解析) /
//! [`codex`] (Codex 转换 + shell 辅助) /
//! [`opencode`] (vendored 插件) / [`scripts`] (外挂脚本) / [`staging`] (staging 预生成) /
//! [`io_util`] (原子写) / [`types`] (输入类型)。

use std::path::Path;

use serde_json::{Map, Value, json};
use tokio::fs;

use crate::error::AppResult;

mod claude;
mod codex;
mod io_util;
mod opencode;
mod parse;
mod scripts;
mod staging;
mod types;

pub use types::{HookConfigInput, HookScript};

use claude::{SettingsUpdate, apply_settings, replace_hook_scripts};
use io_util::write_json_file_atomic;
use opencode::{has_platform_env_script, install_opencode_platform_env_plugin};
use parse::parse_hooks_config_with_status;
use staging::{
    StagedRuntime, apply_staged_runtime_hook_artifacts, clear_hook_artifacts,
    stage_runtime_hook_artifacts,
};

// ── 主入口 (writeAgentHookConfigs) ───────────────────────────────────────────────

/// 写入 Claude Code / Codex / OpenCode 三套 Hook 相关配置 (对齐 nuwax writeAgentHookConfigs)。
///
/// 仅在对应配置解析成功时才清除并重写。文件系统错误立即返回；workspace 创建调用方
/// 可以显式选择 best-effort。staging 目录由 `TempDir` 在所有退出路径自动清理。
pub async fn write_agent_hook_configs(workspace: &Path, opts: HookConfigInput) -> AppResult<()> {
    let HookConfigInput {
        mcp_servers_config,
        hooks_config,
        permissions_config,
        hook_scripts,
    } = opts;

    let has_mcp_input = mcp_servers_config
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let hooks_status = parse_hooks_config_with_status(hooks_config.as_deref());
    let has_perms_input = permissions_config
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_scripts = hook_scripts
        .as_ref()
        .is_some_and(|scripts| !scripts.is_empty());

    if hooks_status.attempted && hooks_status.error.is_some() {
        tracing::error!(
            error = ?hooks_status.error,
            "Invalid hooksConfig, keeping existing hook configs"
        );
    }

    let (mcp_servers, should_update_mcp) = if has_mcp_input {
        match serde_json::from_str::<Value>(mcp_servers_config.as_deref().unwrap_or("")) {
            Ok(v) => (Some(v), true),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse mcpServersConfig, keeping existing .mcp.json");
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    let (permissions, should_update_perms) = if has_perms_input {
        match serde_json::from_str::<Value>(permissions_config.as_deref().unwrap_or("")) {
            Ok(v) => (Some(v), true),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse permissionsConfig, keeping existing permissions");
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    let should_update_hooks = hooks_status.attempted && hooks_status.error.is_none();
    let should_update_scripts = has_scripts;
    let install_platform_env = has_platform_env_script(hook_scripts.as_deref());

    if !should_update_mcp && !should_update_hooks && !should_update_perms && !should_update_scripts
    {
        return Ok(());
    }

    let result = apply_hook_configs(
        workspace,
        PreparedHookUpdate {
            flags: ApplyFlags {
                should_update_mcp,
                should_update_hooks,
                should_update_perms,
                should_update_scripts,
                install_platform_env,
            },
            mcp_servers: mcp_servers.as_ref(),
            hooks_map: hooks_status.hooks_map.as_ref(),
            permissions: permissions.as_ref(),
            hook_scripts: hook_scripts.as_deref(),
        },
    )
    .await;

    // create_workspace 调用方仍可选择 best-effort；本层返回错误，避免底层失败被静默吞掉。
    if let Err(e) = result {
        tracing::error!(
            error = %e,
            "Failed to write agent hook configs, keeping previous files when possible"
        );
        return Err(e);
    }
    Ok(())
}

struct ApplyFlags {
    should_update_mcp: bool,
    should_update_hooks: bool,
    should_update_perms: bool,
    should_update_scripts: bool,
    install_platform_env: bool,
}

struct PreparedHookUpdate<'a> {
    flags: ApplyFlags,
    mcp_servers: Option<&'a Value>,
    hooks_map: Option<&'a Map<String, Value>>,
    permissions: Option<&'a Value>,
    hook_scripts: Option<&'a [HookScript]>,
}

/// 实际 FS 写入逻辑 (对齐 nuwax writeAgentHookConfigs 的 try 块)。
async fn apply_hook_configs(workspace: &Path, update: PreparedHookUpdate<'_>) -> AppResult<()> {
    let PreparedHookUpdate {
        flags,
        mcp_servers,
        hooks_map,
        permissions,
        hook_scripts,
    } = update;
    let claude_dir = workspace.join(".claude");
    let mut staged_runtime: Option<StagedRuntime> = None;

    fs::create_dir_all(&claude_dir).await?;

    // 1. staging 预生成 (仅 hooks 有效且 hooksMap 非空时)
    if flags.should_update_hooks
        && let Some(hm) = hooks_map
    {
        let staged =
            stage_runtime_hook_artifacts(workspace, hm, flags.install_platform_env).await?;
        staged_runtime = Some(staged);
    }

    // 2. .mcp.json
    if flags.should_update_mcp
        && let Some(mcp) = mcp_servers
    {
        write_json_file_atomic(&workspace.join(".mcp.json"), &json!({ "mcpServers": mcp })).await?;
        tracing::info!("Written .mcp.json to workspace root");
    }

    // 3. 应用 / 清理 Codex+OpenCode 运行时产物
    if flags.should_update_hooks && hooks_map.is_some() {
        if let Some(staged) = staged_runtime.as_ref() {
            apply_staged_runtime_hook_artifacts(workspace, staged).await?;
        }
    } else if flags.should_update_hooks && hooks_map.is_none() {
        clear_hook_artifacts(workspace).await?;
    }

    // 4. .claude/settings.json (hooks + permissions)
    if flags.should_update_hooks || flags.should_update_perms {
        apply_settings(
            workspace,
            SettingsUpdate {
                hooks: flags.should_update_hooks.then_some(hooks_map),
                permissions: flags.should_update_perms.then_some(permissions).flatten(),
            },
        )
        .await?;
    }

    // 5. hook 外挂脚本 + 可选 platform-env 插件
    if flags.should_update_scripts {
        if let Some(scripts) = hook_scripts {
            replace_hook_scripts(workspace, scripts).await?;
        }
        if flags.install_platform_env {
            let opencode_plugins_dir = workspace.join(".opencode").join("plugins");
            install_opencode_platform_env_plugin(&opencode_plugins_dir).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode::{
        OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY, OPENCODE_PLUGIN_DIR, OPENCODE_PLUGIN_ENTRY,
    };

    #[tokio::test]
    async fn write_agent_hook_configs_end_to_end() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();

        let hooks = r#"{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"./check.sh"}]},{"hooks":[{"type":"http","url":"https://hook.example.com/cb","timeout":5,"headers":{"X-Token":"$SECRET"}}]}]}"#;
        let mcp = r#"{"filesystem":{"command":"npx","args":["-y","@fs/mcp"]}}"#;
        let perms = r#"{"allow":["Bash(echo:*)"],"deny":[]}"#;
        let scripts = vec![
            HookScript {
                path: "hooks/check.sh".to_string(),
                content: "#!/usr/bin/env bash\necho check\n".to_string(),
            },
            HookScript {
                path: "hooks/platform-env.sh".to_string(),
                content: "#!/usr/bin/env bash\necho env\n".to_string(),
            },
        ];

        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                mcp_servers_config: Some(mcp.to_string()),
                hooks_config: Some(hooks.to_string()),
                permissions_config: Some(perms.to_string()),
                hook_scripts: Some(scripts),
            },
        )
        .await
        .unwrap();

        // .mcp.json
        let mcp_data: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join(".mcp.json")).await.unwrap())
                .unwrap();
        assert_eq!(
            mcp_data
                .get("mcpServers")
                .unwrap()
                .get("filesystem")
                .unwrap()
                .get("command"),
            Some(&json!("npx"))
        );

        // .claude/settings.json: hooks + permissions
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".claude").join("settings.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(settings.get("hooks").unwrap().get("PreToolUse").is_some());
        assert!(settings.get("permissions").unwrap().get("allow").is_some());

        // .codex/hooks.json: http 转 command wrapper
        let codex: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".codex").join("hooks.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        let handlers = codex["hooks"]["PreToolUse"][1]["hooks"].as_array().unwrap();
        assert_eq!(handlers[0]["type"], "command");
        assert!(
            handlers[0]["command"]
                .as_str()
                .unwrap()
                .contains("http-hook-0.sh")
        );
        // wrapper 脚本写入 .codex/hooks/
        assert!(
            tmp.join(".codex")
                .join("hooks")
                .join("http-hook-0.sh")
                .is_file()
        );

        // hook 外挂脚本 (含路径校验, 0o755)
        let check = tmp.join(".claude").join("hooks").join("check.sh");
        assert!(check.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&check).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        // opencode 插件 (platform-env 因含 platform-env.sh 触发)
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLUGIN_ENTRY)
                .is_file()
        );
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLUGIN_DIR)
                .join("dist")
                .join("index.js")
                .is_file()
        );
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY)
                .is_file()
        );

        // staging 目录已清理
        let tmp_dir = tmp.join(".tmp");
        if tmp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&tmp_dir).unwrap().collect();
            assert!(remaining.is_empty(), "staging dir not cleaned");
        }
    }

    #[tokio::test]
    async fn write_agent_hook_configs_invalid_hooks_keeps_existing() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();
        // 预置旧 .mcp.json
        fs::write(
            tmp.join(".mcp.json"),
            "{\"mcpServers\":{\"old\":{\"command\":\"x\"}}}\n",
        )
        .await
        .unwrap();

        // 无效 hooksConfig + 有效 mcp: 应更新 mcp, 不动 hooks 产物
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                mcp_servers_config: Some(r#"{"new":{"command":"y"}}"#.to_string()),
                hooks_config: Some("{bad json".to_string()),
                permissions_config: None,
                hook_scripts: None,
            },
        )
        .await
        .unwrap();

        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join(".mcp.json")).await.unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["new"].is_object());
        // 旧 entry 被覆盖 (mcp 是整文件重写, 对齐 nuwax)
        assert!(mcp["mcpServers"]["old"].is_null());
        // 无 .codex/hooks.json (hooks 无效未触发)
        assert!(!tmp.join(".codex").join("hooks.json").exists());
    }

    #[tokio::test]
    async fn permissions_only_update_preserves_existing_hooks() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                hooks_config: Some(
                    r#"{"Stop":[{"hooks":[{"type":"command","command":"echo stop"}]}]}"#
                        .to_string(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("write hooks");
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                permissions_config: Some(r#"{"allow":["Bash"]}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("write permissions");

        let settings: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".claude/settings.json"))
                .await
                .expect("read settings"),
        )
        .expect("parse settings");
        assert!(settings["hooks"]["Stop"].is_array());
        assert_eq!(settings["permissions"]["allow"][0], "Bash");
        assert!(tmp.join(".codex/hooks.json").is_file());
    }

    #[tokio::test]
    async fn empty_hooks_config_clears_only_hook_artifacts() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                hooks_config: Some(
                    r#"{"Stop":[{"hooks":[{"type":"command","command":"echo stop"}]}]}"#
                        .to_string(),
                ),
                permissions_config: Some(r#"{"allow":["Bash"]}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("write initial hooks");
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                hooks_config: Some("{}".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("clear hooks");

        let settings: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".claude/settings.json"))
                .await
                .expect("read settings"),
        )
        .expect("parse settings");
        assert!(settings.get("hooks").is_none());
        assert_eq!(settings["permissions"]["allow"][0], "Bash");
        assert!(!tmp.join(".codex/hooks.json").exists());
        assert!(
            !tmp.join(".opencode/plugins/opencode-hooks-plugin.js")
                .exists()
        );
    }

    #[tokio::test]
    async fn corrupt_settings_is_preserved_on_permissions_only_update() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();
        let settings_path = tmp.join(".claude/settings.json");
        fs::create_dir_all(settings_path.parent().expect("settings parent"))
            .await
            .expect("create settings directory");
        let corrupt = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command"}]}]}"#;
        fs::write(&settings_path, corrupt)
            .await
            .expect("write corrupt settings");

        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                permissions_config: Some(r#"{"allow":["Bash"]}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("skip corrupt settings");
        assert_eq!(
            fs::read_to_string(settings_path)
                .await
                .expect("read preserved settings"),
            corrupt
        );
    }

    #[tokio::test]
    async fn write_agent_hook_configs_traversal_script_skipped() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let tmp = temp.path();
        fs::create_dir_all(tmp.join(".claude"))
            .await
            .expect("create claude directory");
        fs::write(tmp.join(".claude/settings.json"), "{\"model\":\"sonnet\"}")
            .await
            .expect("write settings fixture");
        write_agent_hook_configs(
            tmp,
            HookConfigInput {
                hook_scripts: Some(vec![
                    HookScript {
                        path: "../escape.sh".to_string(),
                        content: "pwn".to_string(),
                    },
                    HookScript {
                        path: "settings.json".to_string(),
                        content: "overwritten".to_string(),
                    },
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!tmp.parent().unwrap().join("escape.sh").exists());
        assert_eq!(
            fs::read_to_string(tmp.join(".claude/settings.json"))
                .await
                .expect("read preserved settings"),
            "{\"model\":\"sonnet\"}"
        );
    }
}
