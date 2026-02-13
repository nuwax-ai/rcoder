use std::path::PathBuf;
use tracing::{debug, warn};

pub const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;
pub const DETACHED_PROCESS_FLAG: u32 = 0x0000_0008;

fn resolve_windows_node_exe() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("NUWAX_NODE_PATH") {
        let node = PathBuf::from(path);
        if node.exists() {
            debug!(
                "[SACP] Windows node 解析命中 NUWAX_NODE_PATH: {}",
                node.display()
            );
            return Some(node);
        }
    }

    if let Ok(path) = std::env::var("NODE_PATH") {
        let node = PathBuf::from(path);
        if node.exists() {
            debug!(
                "[SACP] Windows node 解析命中 NODE_PATH: {}",
                node.display()
            );
            return Some(node);
        }
    }

    let output = match std::process::Command::new("where")
        .arg("node.exe")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            warn!("[SACP] Windows node 解析失败: where node.exe 执行错误: {}", e);
            return None;
        }
    };
    if !output.status.success() {
        warn!(
            "[SACP] Windows node 解析失败: where node.exe exit={:?}",
            output.status.code()
        );
        return None;
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let node_path = stdout_str
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    debug!("[SACP] Windows node 解析命中 PATH: {}", node_path);
    Some(PathBuf::from(node_path.to_string()))
}

fn npm_package_entry_from_dir(package_dir: &std::path::Path, package_name: &str) -> Option<PathBuf> {
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
            .or_else(|| bin_map.values().find_map(|v| v.as_str()).map(str::to_string))
    } else {
        None
    }?;

    let entry = package_dir.join(rel_entry);
    if entry.exists() {
        Some(entry)
    } else {
        None
    }
}

fn get_windows_cmd_script_path(path: &std::path::Path) -> Option<PathBuf> {
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmd") => Some(path.to_path_buf()),
        None => {
            let candidate = path.with_extension("cmd");
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn resolve_js_entry_from_cmd_shim(cmd_script: &std::path::Path) -> Option<PathBuf> {
    let content = match std::fs::read_to_string(cmd_script) {
        Ok(c) => c,
        Err(e) => {
            debug!(
                "[SACP] cmd shim 读取失败: {} ({})",
                cmd_script.display(),
                e
            );
            return None;
        }
    };
    let base_dir = cmd_script.parent()?;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if !line.contains("%~dp0") {
            continue;
        }
        let clean = line.replace('"', "");
        let clean_lower = clean.to_ascii_lowercase();
        let ext_end = [".cjs", ".mjs", ".js"]
            .iter()
            .filter_map(|ext| clean_lower.find(ext).map(|pos| pos + ext.len()))
            .min();
        let Some(end) = ext_end else {
            continue;
        };
        let start = clean.find("%~dp0")?;
        if end <= start + 5 || end > clean.len() {
            continue;
        }
        let rel = clean[start + 5..end].trim_start_matches(['\\', '/']);
        if rel.is_empty() {
            continue;
        }
        let rel = rel.replace('/', "\\");
        let entry = base_dir.join(std::path::Path::new(&rel));
        if entry.exists() {
            debug!(
                "[SACP] cmd shim 解析命中入口: {} -> {}",
                cmd_script.display(),
                entry.display()
            );
            return Some(entry);
        }
    }

    debug!(
        "[SACP] cmd shim 未解析到入口: {}",
        cmd_script.display()
    );

    None
}

pub fn resolve_windows_node_cli_command(command: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    let command_path = which::which(command).unwrap_or_else(|_| PathBuf::from(command));
    let path = command_path.as_path();
    debug!(
        "[SACP] Windows 命令解析开始: input={}, resolved={}",
        command,
        path.display()
    );
    let cmd_script = get_windows_cmd_script_path(path);

    let node_exe = resolve_windows_node_exe()?;

    if let Some(cmd_script) = cmd_script.as_ref() {
        if let Some(js_entry) = resolve_js_entry_from_cmd_shim(cmd_script) {
            let mut actual_args = Vec::with_capacity(args.len() + 1);
            actual_args.push(js_entry.to_string_lossy().to_string());
            actual_args.extend(args.iter().cloned());
            return Some((node_exe.to_string_lossy().to_string(), actual_args));
        }
        debug!(
            "[SACP] cmd shim 存在但未解析到入口，转 package.json bin 解析: {}",
            cmd_script.display()
        );
    }

    let package_name = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmd") => {
            path.file_stem().and_then(|s| s.to_str()).map(str::to_string)
        }
        None => path.file_name().and_then(|s| s.to_str()).map(str::to_string),
        _ => None,
    }?;

    let mut package_dirs: Vec<PathBuf> = Vec::new();

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

    if let Ok(appdata) = std::env::var("APPDATA") {
        package_dirs.push(
            PathBuf::from(appdata)
                .join("npm")
                .join("node_modules")
                .join(&package_name),
        );
    }

    if let Some(home) = dirs::home_dir() {
        package_dirs.push(
            home.join(".local")
                .join("lib")
                .join("node_modules")
                .join(&package_name),
        );
    }

    for package_dir in package_dirs {
        if !package_dir.exists() {
            debug!(
                "[SACP] package 目录不存在，跳过: {}",
                package_dir.display()
            );
            continue;
        }
        if let Some(js_entry) = npm_package_entry_from_dir(&package_dir, &package_name) {
            let mut actual_args = Vec::with_capacity(args.len() + 1);
            actual_args.push(js_entry.to_string_lossy().to_string());
            actual_args.extend(args.iter().cloned());
            return Some((node_exe.to_string_lossy().to_string(), actual_args));
        }
        debug!(
            "[SACP] package.json bin 解析失败: package={}, dir={}",
            package_name,
            package_dir.display()
        );
    }

    warn!(
        "[SACP] Windows 命令解析失败，未找到可直连 node 的 JS 入口: command={}",
        command
    );
    None
}
