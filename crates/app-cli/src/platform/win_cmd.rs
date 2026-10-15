//! Windows 命令解析：`.cmd`/`.bat` shim 探测补全。
//!
//! Windows 的 CreateProcess 只自动补 `.exe`，不解析 npm/pnpm/yarn 等 Node 生态
//! 工具安装的 `.cmd`/`.bat` shim——`Command::new("npm")` 直接 spawn 会报
//! "program not found"。spawn 用户/manifest 提供的命令前，对无扩展名的裸程序名
//! 按 PATH 探测 `<name>.cmd` / `<name>.bat`。unix 下原样返回（零开销直通）。

/// 解析 spawn 的程序名：Windows 下对无扩展名的裸名探测 `.cmd`/`.bat` shim；
/// 其余情形（含路径分隔符、已有扩展名、非 Windows 平台）原样返回。
pub(crate) fn resolve_spawn_program(program: &str) -> String {
    #[cfg(windows)]
    {
        let p = std::path::Path::new(program);
        if !program.is_empty()
            && p.extension().is_none()
            && !program.contains('/')
            && !program.contains('\\')
        {
            for ext in [".cmd", ".bat"] {
                if let Some(hit) = which_in_path(&format!("{program}{ext}")) {
                    return hit;
                }
            }
        }
    }
    #[allow(unused_variables)]
    program.to_string()
}

#[cfg(windows)]
fn which_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// unix 下直通：无论输入如何都原样返回（本仓库 CI 主战场，锁语义）。
    #[cfg(not(windows))]
    #[test]
    fn passthrough_on_unix() {
        assert_eq!(resolve_spawn_program("npm"), "npm");
        assert_eq!(resolve_spawn_program(""), "");
    }

    /// windows 下含扩展名/路径分隔符的不探测（CreateProcess 能直接解析）。
    #[cfg(windows)]
    #[test]
    fn skips_qualified_names() {
        // 含扩展名或分隔符：原样返回（无 PATH 依赖，恒定行为）
        assert_eq!(resolve_spawn_program("node.exe"), "node.exe");
        assert_eq!(resolve_spawn_program(".\\tools\\build"), ".\\tools\\build");
    }
}
