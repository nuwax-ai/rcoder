//! 版本信息工具模块
//!
//! 提供非阻塞的外部工具版本检查功能，用于服务启动时打印依赖工具的版本信息，
//! 方便排查环境问题。

use tokio::process::Command;
use tracing::{info, warn};

/// 非阻塞执行命令并打印版本信息
///
/// 将版本检查任务 spawn 到 tokio 运行时后台执行，
/// 不阻塞服务启动流程。命令执行失败时仅打印 warn 日志。
///
/// # Arguments
///
/// * `name` - 工具名称，用于日志输出
/// * `cmd` - 要执行的命令及参数，例如 `&["nuwaxcode", "-v"]`
pub fn spawn_tool_version_log(name: &'static str, cmd: &'static [&'static str]) {
    tokio::spawn(async move {
        match Command::new(cmd[0]).args(&cmd[1..]).output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if output.status.success() && !stdout.is_empty() {
                    info!("📦 {} version: {}", name, stdout);
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let detail = if stderr.is_empty() {
                        "no output".to_string()
                    } else {
                        stderr
                    };
                    warn!("📦 {} version check failed: {}", name, detail);
                }
            }
            Err(e) => {
                warn!(
                    "📦 {} version check failed: {} (command may not be installed)",
                    name, e
                );
            }
        }
    });
}
