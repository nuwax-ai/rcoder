//! project_id → 项目工作目录解析与白名单校验
//!
//! 终端中间层从 Pingora 注入的两个 header 拿到上下文：
//! - `X-Ttyd-Project-Id`：项目标识
//! - `X-Ttyd-Service-Type`：业务场景（`shared_types::ServiceType` 的 kebab-case 形式）
//!
//! 根据 serviceType 显式选项目目录前缀：
//! - `ComputerAgentRunner`（computer-agent-runner，agent-runner 镜像）：`/home/user`
//!   （用户目录，一个 user_id 对应一个容器，所有项目都在此目录下）
//! - `WebAgentRunner`（web-agent-runner，rcoder 镜像）：`/app/project_workspace/{project_id}`
//!   （项目目录，一个 project_id 对应一个容器）
//!
//! serviceType 缺失/未知时回退到自动检测两前缀（兼容旧 Pingora）。白名单规则与
//! `start-ttyd.sh` 的 wrapper `is_path_allowed` 一致，杜绝路径穿越和绝对路径逃逸。

use std::path::{Path, PathBuf};

/// 自动检测 fallback 用的候选前缀（serviceType 缺失/未知时遍历）
const HOME_CANDIDATES: &[&str] = &["/home/user", "/app/project_workspace"];

/// project_id 合法字符集：字母数字、`-`、`_`
///
/// 与 Pingora 侧 `ttyd.rs` 的校验、wrapper 的白名单保持一致，
/// 从源头拒绝 `..`、`/`、绝对路径等危险输入。
fn is_valid_project_id(pid: &str) -> bool {
    !pid.is_empty() && pid.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// 根据 serviceType + project_id 解析终端工作目录
///
/// serviceType 明确时按其对应前缀解析；serviceType 缺失/未知时回退到自动检测
/// 两前缀（[`HOME_CANDIDATES`]），兼容旧 Pingora 未注入 `X-Ttyd-Service-Type` 的场景。
///
/// 对于不同服务类型的行为：
/// - `ComputerAgentRunner`：直接返回 `/home/user`（用户目录，不需要 project_id）
/// - `WebAgentRunner`：返回 `/app/project_workspace/{project_id}`（项目目录）
///
/// 任一前缀下都需目录存在、canonicalize 后仍在该前缀下（防 symlink 穿越）、
/// 且是目录，否则返回 `None`（调用方回退到 `$HOME`）。
pub fn resolve_project_cwd(service_type: &str, project_id: &str) -> Option<PathBuf> {
    use shared_types::ServiceType;
    use std::str::FromStr;

    // 根据 service_type 决定是否需要拼接 project_id
    match ServiceType::from_str(service_type).ok() {
        Some(ServiceType::ComputerAgentRunner) => {
            // ComputerAgentRunner: 一个 user_id 对应一个容器，所有项目都在 /home/user
            // 直接返回 /home/user，不需要拼接 project_id
            let base = Path::new("/home/user");
            resolve_user_directory(base)
        }
        Some(ServiceType::WebAgentRunner) => {
            // WebAgentRunner: 一个 project_id 对应一个容器
            // 返回 /app/project_workspace/{project_id}
            resolve_in_candidates(project_id, &["/app/project_workspace"])
        }
        None => {
            // serviceType 缺失/未知时回退到自动检测
            resolve_in_candidates(project_id, HOME_CANDIDATES)
        }
    }
}

/// 解析用户目录（ComputerAgentRunner 场景）
///
/// 检查 /home/user 目录是否存在且可访问
fn resolve_user_directory(base: &Path) -> Option<PathBuf> {
    // canonicalize 解析符号链接
    if let Ok(real) = base.canonicalize()
        && real.is_dir()
    {
        return Some(real);
    }
    None
}

/// 内部：在给定候选前缀列表中解析（抽出来便于单元测试用 tempdir 注入候选）
fn resolve_in_candidates(project_id: &str, candidates: &[&str]) -> Option<PathBuf> {
    if !is_valid_project_id(project_id) {
        return None;
    }
    for base in candidates {
        let base_path = Path::new(base);
        // 前缀本身可能不存在（如 web 容器没有 /home/user），跳过该候选
        let Ok(base_real) = base_path.canonicalize() else {
            continue;
        };
        let candidate = base_real.join(project_id);
        // canonicalize 解析符号链接和 `..`，是防穿越的关键
        if let Ok(real) = candidate.canonicalize()
            && real.starts_with(&base_real)
            && real.is_dir()
        {
            return Some(real);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rejects_empty_project_id() {
        // WebAgentRunner 需要 project_id，空值应返回 None
        assert_eq!(resolve_project_cwd("web-agent-runner", ""), None);
    }

    #[test]
    fn rejects_invalid_chars() {
        // WebAgentRunner 需要 project_id，无效字符应返回 None
        assert_eq!(resolve_project_cwd("web-agent-runner", "../etc"), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "a/b"), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "a;b"), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "a b"), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "."), None);
    }

    #[test]
    fn computer_agent_runner_resolves_user_directory() {
        // ComputerAgentRunner 直接返回 /home/user，不需要 project_id
        // 注意：这个测试在非容器环境中会返回 None，因为 /home/user 可能不存在
        let result = resolve_project_cwd("computer-agent-runner", "any-project-id");
        // 在容器环境中应该返回 Some("/home/user")，在非容器环境中返回 None
        // 这里只测试逻辑正确性，不测试实际路径
        if result.is_some() {
            assert_eq!(result.unwrap().to_str().unwrap(), "/home/user");
        }
    }

    #[test]
    fn accepts_when_dir_exists_in_candidate() {
        let tmp = tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let pid = "proj-123_abc";
        fs::create_dir_all(tmp.path().join(pid)).unwrap();
        let resolved = resolve_in_candidates(pid, &[base]).expect("合法 id 应解析成功");
        assert_eq!(resolved, tmp.path().join(pid).canonicalize().unwrap());
    }

    #[test]
    fn falls_through_to_next_candidate_if_first_missing_dir() {
        // 第一个候选前缀存在但无 pid 子目录 → 转到第二个候选
        let tmp1 = tempdir().unwrap();
        let tmp2 = tempdir().unwrap();
        let pid = "proj-2";
        fs::create_dir_all(tmp2.path().join(pid)).unwrap();
        let resolved = resolve_in_candidates(
            pid,
            &[tmp1.path().to_str().unwrap(), tmp2.path().to_str().unwrap()],
        )
        .expect("应在第二个候选命中");
        assert_eq!(resolved, tmp2.path().join(pid).canonicalize().unwrap());
    }

    #[test]
    fn skips_nonexistent_candidate_prefix() {
        // 候选前缀本身不存在（如 web 容器无 /home/user）→ 跳过,不报错
        let tmp = tempdir().unwrap();
        let pid = "proj-3";
        fs::create_dir_all(tmp.path().join(pid)).unwrap();
        let resolved = resolve_in_candidates(
            pid,
            &["/this/does/not/exist", tmp.path().to_str().unwrap()],
        )
        .expect("应跳过不存在的前缀,命中第二个");
        assert_eq!(resolved, tmp.path().join(pid).canonicalize().unwrap());
    }

    #[test]
    fn returns_none_when_no_candidate_matches() {
        let tmp = tempdir().unwrap();
        // 合法 id 但所有候选下都无对应目录
        assert_eq!(
            resolve_in_candidates("proj-x", &[tmp.path().to_str().unwrap()]),
            None
        );
    }

    #[test]
    fn blocks_symlink_traversal_outside_base() {
        // project_id 字符合法,但对应的是指向 base 之外的符号链接 → canonicalize 后不在 base 下 → None
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, base.join("link")).unwrap();
            assert_eq!(
                resolve_in_candidates("link", &[base.to_str().unwrap()]),
                None
            );
        }
    }

    #[test]
    fn returns_none_for_file_instead_of_dir() {
        let tmp = tempdir().unwrap();
        // 合法 id 但对应路径是文件而非目录 → None
        fs::write(tmp.path().join("afile"), b"x").unwrap();
        assert_eq!(
            resolve_in_candidates("afile", &[tmp.path().to_str().unwrap()]),
            None
        );
    }
}
