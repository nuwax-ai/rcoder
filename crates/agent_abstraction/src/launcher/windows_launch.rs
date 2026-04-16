use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

pub const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;

fn resolve_windows_node_exe() -> Option<PathBuf> {
    // 1. 显式指定的 node 可执行文件路径（最高优先级）
    if let Ok(path) = std::env::var("NUWAX_NODE_PATH") {
        let node = PathBuf::from(path);
        if node.exists() {
            info!(
                "[SACP] Windows node resolved via NUWAX_NODE_PATH: {}",
                node.display()
            );
            return Some(node);
        }
    }

    // 1.5 Tauri 层设置的 NUWAX_NODE_EXE（sidecar node-runtime.exe）
    if let Ok(path) = std::env::var("NUWAX_NODE_EXE") {
        let node = PathBuf::from(&path);
        if node.exists() {
            info!(
                "[SACP] Windows node resolved via NUWAX_NODE_EXE: {}",
                node.display()
            );
            return Some(node);
        }
    }

    // 2. 应用集成的 runtime 路径（Tauri 打包后设置的 NUWAX_APP_RUNTIME_PATH，
    //    包含 runtime\node\bin 等目录，内含集成的 node.exe）
    if let Ok(runtime_path) = std::env::var("NUWAX_APP_RUNTIME_PATH") {
        for dir in runtime_path.split(';') {
            let dir = dir.trim();
            if dir.is_empty() {
                continue;
            }
            let node = PathBuf::from(dir).join("node.exe");
            if node.exists() {
                info!(
                    "[SACP] Windows node resolved via NUWAX_APP_RUNTIME_PATH: {}",
                    node.display()
                );
                return Some(node);
            }
        }
    }

    // 3. 从 APPDATA 推导默认的应用 runtime 路径（NUWAX_APP_RUNTIME_PATH 未设置时的回退）
    if let Ok(appdata) = std::env::var("APPDATA") {
        let default_node = PathBuf::from(&appdata)
            .join("com.nuwax.agent-tauri-client")
            .join("runtime")
            .join("node")
            .join("bin")
            .join("node.exe");
        if default_node.exists() {
            info!(
                "[SACP] Windows node resolved via APPDATA default path: {}",
                default_node.display()
            );
            return Some(default_node);
        }
    }

    warn!(
        "[SACP] Windows node resolution failed: NUWAX_NODE_PATH, NUWAX_NODE_EXE, NUWAX_APP_RUNTIME_PATH and APPDATA default paths all missed"
    );
    None
}

fn npm_package_entry_from_dir(
    package_dir: &std::path::Path,
    package_name: &str,
) -> Option<PathBuf> {
    let package_json = package_dir.join("package.json");
    let content = std::fs::read_to_string(package_json).ok()?;
    let package_json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let bin_field = package_json.get("bin")?;

    let rel_entry = if let Some(bin_str) = bin_field.as_str() {
        Some(bin_str.to_string())
    } else if let Some(bin_map) = bin_field.as_object() {
        bin_map
            .get(package_name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                bin_map
                    .values()
                    .find_map(|v| v.as_str())
                    .map(str::to_string)
            })
    } else {
        None
    }?;

    let entry = package_dir.join(rel_entry);
    if entry.exists() { Some(entry) } else { None }
}

fn get_windows_cmd_script_path(path: &std::path::Path) -> Option<PathBuf> {
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmd") => Some(path.to_path_buf()),
        None => {
            // 先检查直接路径（绝对路径 or 当前目录）
            let direct_candidate = path.with_extension("cmd");
            if direct_candidate.exists() {
                return Some(direct_candidate);
            }

            // which 未命中时 path 为裸命令名，需要在已知目录中搜索
            let program_name = path.file_name().and_then(|s| s.to_str())?;

            // 搜索应用自管的 node_modules/.bin
            if let Ok(appdata) = std::env::var("APPDATA") {
                let app_private = PathBuf::from(&appdata)
                    .join("com.nuwax.agent-tauri-client")
                    .join("node_modules")
                    .join(".bin")
                    .join(format!("{}.cmd", program_name));
                if app_private.exists() {
                    return Some(app_private);
                }
            }

            // 搜索 NUWAX_APP_RUNTIME_PATH 中的每个目录
            if let Ok(runtime_path) = std::env::var("NUWAX_APP_RUNTIME_PATH") {
                for dir in runtime_path.split(';').filter(|s| !s.trim().is_empty()) {
                    let candidate =
                        std::path::Path::new(dir.trim()).join(format!("{}.cmd", program_name));
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }

            None
        }
        _ => None,
    }
}

fn resolve_js_entry_from_cmd_shim(cmd_script: &std::path::Path) -> Option<PathBuf> {
    let content = match std::fs::read_to_string(cmd_script) {
        Ok(c) => c,
        Err(e) => {
            debug!(
                "[SACP] cmd shim read failed: {} ({})",
                cmd_script.display(),
                e
            );
            return None;
        }
    };
    let base_dir = cmd_script.parent()?;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        // 支持多种 npm shim 格式：%~dp0（旧版）、%dp0%（新版 npm >=7）、%dp0（变体）
        let base_token = if line.contains("%~dp0") {
            "%~dp0"
        } else if line.contains("%dp0%") {
            "%dp0%"
        } else if line.contains("%dp0") {
            "%dp0"
        } else {
            continue;
        };
        let clean = line.replace('"', "");
        let clean_lower = clean.to_ascii_lowercase();
        let ext_end = [".cjs", ".mjs", ".js"]
            .iter()
            .filter_map(|ext| clean_lower.find(ext).map(|pos| pos + ext.len()))
            .min();
        let Some(end) = ext_end else {
            continue;
        };
        let start = clean.find(base_token)?;
        if end <= start + base_token.len() || end > clean.len() {
            continue;
        }
        let rel = clean[start + base_token.len()..end].trim_start_matches(['\\', '/']);
        if rel.is_empty() {
            continue;
        }
        let rel = rel.replace('/', "\\");
        let entry = base_dir.join(std::path::Path::new(&rel));
        if entry.exists() {
            debug!(
                "[SACP] cmd shim resolved entry: {} -> {}",
                cmd_script.display(),
                entry.display()
            );
            return Some(entry);
        }
    }

    debug!("[SACP] cmd shim not resolved: {}", cmd_script.display());

    None
}

pub fn resolve_windows_node_cli_command(
    command: &str,
    args: &[String],
) -> Option<(String, Vec<String>)> {
    let command_path = which::which(command).unwrap_or_else(|_| PathBuf::from(command));
    let path = command_path.as_path();
    info!(
        "[SACP] Windows command resolution started: input={}, resolved={}",
        command,
        path.display()
    );
    let cmd_script = get_windows_cmd_script_path(path);

    let node_exe = resolve_windows_node_exe()?;
    info!(
        "[SACP] Windows node.exe already resolved: {}",
        node_exe.display()
    );

    if let Some(cmd_script) = cmd_script.as_ref() {
        info!("[SACP] using cmd shim: {}", cmd_script.display());
        if let Some(js_entry) = resolve_js_entry_from_cmd_shim(cmd_script) {
            let mut actual_args = Vec::with_capacity(args.len() + 1);
            actual_args.push(js_entry.to_string_lossy().to_string());
            actual_args.extend(args.iter().cloned());
            info!(
                "[SACP] cmd shim resolution succeeded: {} -> {}",
                cmd_script.display(),
                js_entry.display()
            );
            return Some((node_exe.to_string_lossy().to_string(), actual_args));
        }
        info!(
            "[SACP] cmd shim exists but JS entry not resolved, falling back to package.json bin: {}",
            cmd_script.display()
        );
    } else {
        info!(
            "[SACP] cmd shim not found, falling back to package.json bin: command={}",
            command
        );
    }

    let package_name = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmd") => path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string),
        None => path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string),
        _ => None,
    }?;

    let mut package_dirs: Vec<PathBuf> = Vec::new();

    // 从 which 解析到的 .bin 目录向上推导 node_modules
    if let Some(parent) = path.parent() {
        let is_npm_bin = parent
            .file_name()
            .map(|s| s.to_string_lossy().eq_ignore_ascii_case(".bin"))
            .unwrap_or(false);
        if is_npm_bin {
            if let Some(node_modules_dir) = parent.parent() {
                package_dirs.push(node_modules_dir.join(&package_name));
            }
        }
    }

    // 仅搜索应用自管的 node_modules
    if let Ok(appdata) = std::env::var("APPDATA") {
        package_dirs.push(
            PathBuf::from(appdata)
                .join("com.nuwax.agent-tauri-client")
                .join("node_modules")
                .join(&package_name),
        );
    }

    for package_dir in package_dirs {
        if !package_dir.exists() {
            info!(
                "[SACP] package directorynot found, skip: {}",
                package_dir.display()
            );
            continue;
        }
        if let Some(js_entry) = npm_package_entry_from_dir(&package_dir, &package_name) {
            let mut actual_args = Vec::with_capacity(args.len() + 1);
            actual_args.push(js_entry.to_string_lossy().to_string());
            actual_args.extend(args.iter().cloned());
            info!(
                "[SACP] package.json bin resolved: {} -> {}",
                package_name,
                js_entry.display()
            );
            return Some((node_exe.to_string_lossy().to_string(), actual_args));
        }
        info!(
            "[SACP] package.json bin resolution failed: package={}, dir={}",
            package_name,
            package_dir.display()
        );
    }

    warn!(
        "[SACP] Windows command resolution failed, no JS entry found: command={}",
        command
    );
    None
}

/// Windows 下将「命令 + 参数」规范化为不弹 CMD 窗口的形式。
///
/// 背景：Windows 上 npm 全局安装会创建 .cmd/.bat，直接执行会弹出 CMD 窗口。
/// 本函数按扩展名检测并尽可能转换为 `node.exe + JS 入口`，避免弹窗。
///
/// 检查顺序（从优到劣）：
/// 1. `.exe` → 原生二进制，直接返回（不修改）
/// 2. `.cmd` / `.bat` → 尝试解析为 node.exe + JS，成功则返回新 (path, args)
/// 3. 其他扩展名 → 直接返回
/// 4. 无扩展名 → 用 `which` 解析后再按 1–3 处理
///
/// 仅编译于 Windows；调用方需使用 `#[cfg(windows)]`。
#[cfg(windows)]
pub fn normalize_windows_command_for_no_window(
    command_path: String,
    command_args: Vec<String>,
) -> (String, Vec<String>) {
    let mut path = command_path;
    let mut args = command_args;

    let path_ref = Path::new(&path);
    let ext = path_ref
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());

    match ext.as_deref() {
        Some("exe") => {
            info!(
                "[SACP] Windows detected native .exe: {} - no popup window",
                path
            );
        }
        Some("cmd" | "bat") => {
            info!("[SACP] 🔍 Windows detecting .cmd/.bat: {}", path);
            info!("[SACP] 🔄 converting to node.exe + JS ...");
            if let Some((node_path, js_args)) = resolve_windows_node_cli_command(&path, &args) {
                info!("[SACP] conversion succeeded: {} + {:?}", node_path, js_args);
                path = node_path;
                args = js_args;
            } else {
                warn!(
                    "[SACP] ⚠️ conversion failed, will run original command (may show popup): {}",
                    path
                );
            }
        }
        Some(other) => {
            info!(
                "[SACP] ℹ️ Windows detected other format .{}: {} - trying direct execution",
                other, path
            );
        }
        None => {
            info!("[SACP] 🔍 Windows detecting extension: {}", path);
            if let Ok(resolved) = which::which(&path) {
                let resolved_str = resolved.to_string_lossy().to_string();
                info!("[SACP] 🔄 resolved: {}", resolved_str);

                let resolved_ext = resolved
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_ascii_lowercase());

                match resolved_ext.as_deref() {
                    Some("exe") => {
                        info!("[SACP] detected .exe - no popup window");
                        path = resolved_str;
                    }
                    Some("cmd" | "bat") => {
                        info!("[SACP] 🔍 detected .cmd/.bat, converting ...");
                        if let Some((node_path, js_args)) =
                            resolve_windows_node_cli_command(&resolved_str, &args)
                        {
                            info!("[SACP] conversion succeeded: {} + {:?}", node_path, js_args);
                            path = node_path;
                            args = js_args;
                        } else {
                            warn!("[SACP] ⚠️ conversion failed");
                        }
                    }
                    _ => {
                        info!("[SACP] ℹ️ unknown extension, keeping original");
                        path = resolved_str;
                    }
                }
            } else {
                warn!("[SACP] ⚠️ unable to resolve path: {}", path);
            }
        }
    }

    (path, args)
}
