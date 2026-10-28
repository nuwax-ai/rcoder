//! Delegate native tool lookup to the shared platform resolver.

/// 解析 spawn 的程序名：Windows 下对无扩展名的裸名探测 `.cmd`/`.bat` shim；
/// 其余情形（含路径分隔符、已有扩展名、非 Windows 平台）原样返回。
pub(crate) use process_utils::resolve_spawn_program;

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
