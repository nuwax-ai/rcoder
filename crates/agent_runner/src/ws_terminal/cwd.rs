//! project_id → 项目工作目录解析与白名单校验
//!
//! 终端中间层从 Pingora 注入的 header 拿到上下文：
//! - `X-Ttyd-Project-Id`：项目标识
//! - `X-Ttyd-Service-Type`：业务场景（`shared_types::ServiceType` 的 kebab-case 形式）
//! - `X-Ttyd-Tenant-Id` / `X-Ttyd-Space-Id`：项目归属（Pingora 按 project_id 反查注入，
//!   见 `ContainerLookup::find_project_scope`）。共享容器（tenant/space 隔离）下用于
//!   拼接三级工作目录。
//!
//! 根据 serviceType + tenant/space 解析项目目录前缀：
//! - `ComputerAgentRunner`（computer-agent-runner 镜像）：`/home/user/{project_id}`
//!   （per-user 容器，不受 tenant/space 影响）
//! - `WebAgentRunner`（web-agent-runner，rcoder 镜像）：
//!   - 共享容器（tenant/space 齐全）：`/app/project_workspace/{tenant}/{space}/{project_id}`
//!   - 单项目隔离（缺 tenant/space）：`/app/project_workspace/{project_id}`
//!
//! tenant/space 缺失/非法时安全降级为单级。白名单规则与 `start-ttyd.sh` 的 wrapper
//! `is_path_allowed` 一致，杜绝路径穿越和绝对路径逃逸。

use std::path::{Path, PathBuf};

use shared_types::paths::WORKSPACE_ROOT;

/// ComputerAgentRunner 容器内项目目录前缀（per-user 容器，不受隔离模式影响）
const HOME_PREFIX: &str = "/home/user";
/// userApp 开发卷根（UserappBuilder 终端 cwd 前缀）。
const USERAPP_PREFIX: &str = shared_types::paths::USERAPP_WORKSPACE_ROOT;

// WebAgentRunner 工作区根用 shared_types::paths::WORKSPACE_ROOT (单一事实源, 不再本地定义)

/// project_id 合法字符集：字母数字、`-`、`_`
///
/// 与 Pingora 侧 `ttyd.rs` 的校验、wrapper 的白名单保持一致，
/// 从源头拒绝 `..`、`/`、绝对路径等危险输入。
fn is_valid_project_id(pid: &str) -> bool {
    !pid.is_empty()
        && pid
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// 根据 serviceType + project_id（+ tenant/space）解析终端工作目录
///
/// tenant/space 由 Pingora 按 project_id 反查注入（`X-Ttyd-Tenant-Id`/`X-Ttyd-Space-Id`）：
/// - `WebAgentRunner` 共享容器（tenant/space 齐全）：三级 `/app/project_workspace/{t}/{s}/{pid}`
/// - `WebAgentRunner` 单项目隔离（缺 tenant/space）：单级 `/app/project_workspace/{pid}`
/// - `ComputerAgentRunner`：`/home/user/{pid}`（不受 tenant/space 影响）
///
/// serviceType 缺失/未知时回退到自动检测（兼容旧 Pingora）。任一前缀下都需目录存在、
/// canonicalize 后仍在该前缀下（防 symlink 穿越）、且是目录，否则返回 `None`（调用方
/// 回退到 `$HOME`）。
pub fn resolve_project_cwd(
    service_type: &str,
    project_id: &str,
    tenant_id: &str,
    space_id: &str,
) -> Option<PathBuf> {
    use shared_types::ServiceType;
    use std::str::FromStr;

    // 空串视同未注入（Pingora 反查失败时不注入 header → 这里收到 ""）
    let tenant = (!tenant_id.is_empty()).then_some(tenant_id);
    let space = (!space_id.is_empty()).then_some(space_id);

    match ServiceType::from_str(service_type).ok() {
        Some(ServiceType::ComputerAgentRunner) => {
            // per-user 容器：项目目录恒为单级 /home/user/{project_id}
            resolve_in_candidates(project_id, &[HOME_PREFIX])
        }
        Some(ServiceType::UserappBuilder) => {
            // userApp 开发容器的终端：workspace = 开发卷 {USERAPP_WORKSPACE_ROOT}/{app_id}
            // （与 chat 的 work_dir、file-server 的 resolve_userapp_dev 同根——
            // /userapp/dev/ttyd/{user_id}/{app_id} 经 Pingora 注入本 service_type 到达此处）
            resolve_in_candidates(project_id, &[USERAPP_PREFIX])
        }
        Some(ServiceType::WebAgentRunner) | Some(ServiceType::Userapp) => {
            // 共享容器三级优先，单项目隔离单级兜底
            // Userapp 不由 agent_runner 托管（app_manager Deployment 路径）
            let prefixes = build_web_prefixes(tenant, space);
            let refs: Vec<&str> = prefixes.iter().map(String::as_str).collect();
            resolve_in_candidates(project_id, &refs)
        }
        None => {
            // serviceType 缺失/未知（兼容旧 Pingora）：computer + web 两类前缀都试
            let web_prefixes = build_web_prefixes(tenant, space);
            let mut refs: Vec<&str> = vec![HOME_PREFIX];
            refs.extend(web_prefixes.iter().map(String::as_str));
            resolve_in_candidates(project_id, &refs)
        }
    }
}

/// 构造 WebAgentRunner 工作区候选前缀（与 `grpc::chat` 的 project_dir 解析对齐）。
///
/// tenant/space 均经 `is_valid_project_id` 校验合法 → 三级
/// `/app/project_workspace/{tenant}/{space}` 置首；任一缺失/非法 → 跳过三级。
/// 单级 `/app/project_workspace` 始终兜底。三级在前，命中即短路。
fn build_web_prefixes(tenant: Option<&str>, space: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let valid_pair = tenant
        .filter(|s| is_valid_project_id(s))
        .zip(space.filter(|s| is_valid_project_id(s)));
    if let Some((t, s)) = valid_pair {
        out.push(format!("{WORKSPACE_ROOT}/{t}/{s}"));
    }
    out.push(WORKSPACE_ROOT.to_string());
    out
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
        assert_eq!(resolve_project_cwd("web-agent-runner", "", "", ""), None);
    }

    #[test]
    fn rejects_invalid_chars() {
        // WebAgentRunner 需要 project_id，无效字符应返回 None
        assert_eq!(
            resolve_project_cwd("web-agent-runner", "../etc", "", ""),
            None
        );
        assert_eq!(resolve_project_cwd("web-agent-runner", "a/b", "", ""), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "a;b", "", ""), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", "a b", "", ""), None);
        assert_eq!(resolve_project_cwd("web-agent-runner", ".", "", ""), None);
    }

    #[test]
    fn computer_agent_runner_resolves_user_directory() {
        // ComputerAgentRunner 返回 /home/user/{project_id}（不受 tenant/space 影响）
        // 注意：这个测试在非容器环境中会返回 None，因为 /home/user/{project_id} 可能不存在
        let result = resolve_project_cwd("computer-agent-runner", "1553211", "t", "s");
        // 在容器环境中应该返回 Some("/home/user/1553211")，在非容器环境中返回 None
        // 这里只测试逻辑正确性，不测试实际路径
        if let Some(cwd) = result.as_ref() {
            assert_eq!(cwd.to_str().unwrap(), "/home/user/1553211");
        }
    }

    #[test]
    fn web_prefixes_three_level_when_tenant_space_valid() {
        // tenant/space 均合法 → 三级置首 + 单级兜底
        let p = build_web_prefixes(Some("1"), Some("1184"));
        assert_eq!(
            p,
            vec![
                "/app/project_workspace/1/1184".to_string(),
                "/app/project_workspace".to_string()
            ]
        );
    }

    #[test]
    fn web_prefixes_fallback_single_when_missing() {
        // 任一缺失 → 仅单级
        assert_eq!(
            build_web_prefixes(None, Some("1184")),
            vec!["/app/project_workspace".to_string()]
        );
        assert_eq!(
            build_web_prefixes(Some("1"), None),
            vec!["/app/project_workspace".to_string()]
        );
        assert_eq!(
            build_web_prefixes(None, None),
            vec!["/app/project_workspace".to_string()]
        );
    }

    #[test]
    fn web_prefixes_fallback_when_invalid() {
        // 非法字符（含 /、..、空格）→ 视同缺失，仅单级，杜绝路径穿越
        assert_eq!(
            build_web_prefixes(Some(".."), Some("1184")),
            vec!["/app/project_workspace".to_string()]
        );
        assert_eq!(
            build_web_prefixes(Some("1"), Some("a/b")),
            vec!["/app/project_workspace".to_string()]
        );
        assert_eq!(
            build_web_prefixes(Some("1 2"), Some("3")),
            vec!["/app/project_workspace".to_string()]
        );
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
        let resolved =
            resolve_in_candidates(pid, &["/this/does/not/exist", tmp.path().to_str().unwrap()])
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
