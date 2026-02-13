//! 子进程环境变量管理 - 供 Agent 子进程使用
//!
//! 提供 PATH 环境变量构建函数，供 rcoder 启动 Claude Code 等 Agent 时使用。

use std::path::PathBuf;

/// 在系统 PATH 中查找可执行文件
fn find_in_path(executable: &str) -> Option<String> {
    let output = if cfg!(windows) {
        std::process::Command::new("where")
            .arg(executable)
            .output()
    } else {
        std::process::Command::new("which")
            .arg(executable)
            .output()
    };

    match output {
        Ok(o) if o.status.success() => {
            let binding = String::from_utf8_lossy(&o.stdout);
            binding
                .lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
        _ => None,
    }
}

/// 在系统 PATH 中查找可执行文件并返回其父目录
fn find_bin_dir(executable: &str) -> Option<String> {
    find_in_path(executable).and_then(|path| {
        let parent = PathBuf::from(&path).parent()?.to_path_buf();
        let parent_str = parent.to_string_lossy().to_string();

        // 如果父目录是 "cmd"，尝试找同级的 "bin" 目录
        // 例如: D:\Program Files\Git\cmd -> D:\Program Files\Git\bin
        if parent_str.to_lowercase().ends_with("cmd") {
            let bin_dir = parent
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    if n.eq_ignore_ascii_case("cmd") {
                        parent
                            .parent()
                            .map(|p| p.join("bin").to_string_lossy().to_string())
                    } else {
                        None
                    }
                })
                .flatten();
            if let Some(bin) = bin_dir {
                if PathBuf::from(&bin).join("bash.exe").exists() {
                    return Some(bin);
                }
            }
        }

        Some(parent_str)
    })
}

/// Windows: 查找 Git Bash 路径
///
/// 查找系统 PATH 中的 bash.exe，返回其 bin 目录路径
/// 如果 bash.exe 在 Git\cmd 目录下，会自动查找同级的 Git\bin 目录
#[cfg(windows)]
pub fn find_git_bash_path() -> Option<String> {
    find_bin_dir("bash.exe")
}

/// 构建适用于 rcoder 进程的 PATH 环境变量。
///
/// 优先级：
/// 1. NUWAX_APP_RUNTIME_PATH（优先）
/// 2. APPDATA 中的 node bin 和 npm bin（回退）
///
/// 返回 PATH 字符串，如果都未找到则返回 None
#[cfg(windows)]
pub fn build_rcoder_path_env() -> Option<String> {
    use tracing::info;

    let sep = ";";

    // 1. 优先从 NUWAX_APP_RUNTIME_PATH 构建
    if let Ok(runtime_path) = std::env::var("NUWAX_APP_RUNTIME_PATH") {
        let runtime_path = runtime_path.trim().to_string();
        if !runtime_path.is_empty() {
            info!("[Env] 已从 NUWAX_APP_RUNTIME_PATH 构建 PATH");
            return Some(runtime_path);
        }
    }

    // 2. 从 APPDATA 推导默认路径
    if let Ok(appdata) = std::env::var("APPDATA") {
        let app_base = PathBuf::from(&appdata).join("com.nuwax.agent-tauri-client");
        let mut paths: Vec<String> = Vec::new();

        let node_bin = app_base.join("runtime").join("node").join("bin");
        if node_bin.exists() {
            paths.push(node_bin.to_string_lossy().to_string());
        }

        let npm_bin = app_base.join("node_modules").join(".bin");
        if npm_bin.exists() {
            paths.push(npm_bin.to_string_lossy().to_string());
        }

        if !paths.is_empty() {
            let path_value = paths.join(sep);
            info!("[Env] 从 APPDATA 构建 PATH: {}", path_value);
            return Some(path_value);
        }
    }

    None
}
