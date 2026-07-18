//! Claude Code Hook 配置。
//!
//! 负责合并 `.claude/settings.json` 中的 `hooks` / `permissions`，以及替换
//! `.claude/hooks/` 外挂脚本。Codex 与 OpenCode 的派生产物由各自模块处理。

use std::path::Path;

use serde_json::{Map, Value};
use tokio::fs;

use crate::error::AppResult;
use crate::service::fs_util::path_exists;

use super::io_util::{remove_path_if_exists, write_json_file_atomic};
use super::scripts::write_hook_scripts;
use super::types::HookScript;

/// `None` 表示不更新；`Some(None)` 表示清除；`Some(Some(...))` 表示替换。
pub(super) struct SettingsUpdate<'a> {
    pub hooks: Option<Option<&'a Map<String, Value>>>,
    pub permissions: Option<&'a Value>,
}

struct ClaudeSettings {
    values: Map<String, Value>,
    corrupt: bool,
}

/// 不存在时返回空配置；语法错误或根节点不是对象时标记为损坏。
/// 文件系统错误直接返回，避免把权限/I/O 问题误判成“不存在”。
async fn read_settings(settings_path: &Path) -> AppResult<ClaudeSettings> {
    if !path_exists(settings_path).await? {
        return Ok(ClaudeSettings {
            values: Map::new(),
            corrupt: false,
        });
    }

    let content = fs::read_to_string(settings_path).await?;
    let parsed = serde_json::from_str::<Value>(&content);
    match parsed {
        Ok(Value::Object(values)) => Ok(ClaudeSettings {
            values,
            corrupt: false,
        }),
        Ok(_) | Err(_) => Ok(ClaudeSettings {
            values: Map::new(),
            corrupt: true,
        }),
    }
}

/// 合并 Claude Code settings，保留未参与本次更新的其他字段。
pub(super) async fn apply_settings(workspace: &Path, update: SettingsUpdate<'_>) -> AppResult<()> {
    let claude_dir = workspace.join(".claude");
    let settings_path = claude_dir.join("settings.json");
    fs::create_dir_all(&claude_dir).await?;

    let current = read_settings(&settings_path).await?;
    if current.corrupt && update.hooks.is_none() {
        tracing::warn!(
            settings = %settings_path.display(),
            "Corrupt .claude/settings.json, skipping settings update"
        );
        return Ok(());
    }
    if current.corrupt && update.permissions.is_some() {
        tracing::warn!(
            settings = %settings_path.display(),
            "Corrupt .claude/settings.json, rewriting hooks without preserving permissions"
        );
    }

    let mut next = current.values;
    if let Some(hooks) = update.hooks {
        match hooks {
            Some(hooks) => {
                next.insert("hooks".to_string(), Value::Object(hooks.clone()));
            }
            None => {
                next.remove("hooks");
            }
        }
    }
    // 对齐 nuwax：损坏 settings 被 hooks 更新重建时，不写入 permissions，避免
    // 在无法确认旧结构的情况下把两个独立更新合并到一份被修复的配置中。
    if !current.corrupt
        && let Some(permissions) = update.permissions
    {
        next.insert("permissions".to_string(), permissions.clone());
    }

    if next.is_empty() {
        remove_path_if_exists(&settings_path).await
    } else {
        write_json_file_atomic(&settings_path, &Value::Object(next)).await?;
        tracing::info!("Written .claude/settings.json");
        Ok(())
    }
}

/// 用本次请求的脚本集合替换 `.claude/hooks/`。
pub(super) async fn replace_hook_scripts(
    workspace: &Path,
    hook_scripts: &[HookScript],
) -> AppResult<()> {
    let claude_dir = workspace.join(".claude");
    remove_path_if_exists(&claude_dir.join("hooks")).await?;
    write_hook_scripts(&claude_dir, hook_scripts).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn settings_update_preserves_unrelated_fields() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let settings_path = temp.path().join(".claude/settings.json");
        fs::create_dir_all(settings_path.parent().expect("settings parent"))
            .await
            .expect("create settings directory");
        fs::write(
            &settings_path,
            r#"{"model":"sonnet","permissions":{"old":true}}"#,
        )
        .await
        .expect("write fixture");
        let hooks = json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "x"}]}]});
        let hooks = hooks.as_object().expect("hooks object");

        apply_settings(
            temp.path(),
            SettingsUpdate {
                hooks: Some(Some(hooks)),
                permissions: Some(&json!({"allow": ["Bash"]})),
            },
        )
        .await
        .expect("apply settings");

        let value: Value = serde_json::from_str(
            &fs::read_to_string(settings_path)
                .await
                .expect("read settings"),
        )
        .expect("parse settings");
        assert_eq!(value["model"], "sonnet");
        assert_eq!(value["permissions"]["allow"][0], "Bash");
        assert!(value["hooks"]["PreToolUse"].is_array());
    }
}
