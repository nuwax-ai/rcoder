//! deploy-host Reach 寻址模式解析（配置面 → 进程级模式槽）。
//!
//! 三级解析：**env [`REACH_ENV`] > config `deploy_host.reach` > auto 检测**。
//! 模式在容器创建前进程级决定一次（Docker 创建后不能补绑端口），
//! 之后经 `reach_mode`/`is_direct` 读单例——OnceLock 槽先例 = FeatureFlags
//! （constants.rs）。
//!
//! auto 检测按生效 Docker socket 路径特征判定容器网段从宿主机是否可路由：
//! OrbStack（`.orbstack`）/原生 Linux → Direct（容器 IPv4 直拨，零端口发布）；
//! Docker Desktop（`com.docker.docker`/`docker.raw`）→ Published（虚拟网络对
//! 宿主机不可路由，发布端口是唯一可达路径）；未知 → Published 安全默认。
//!
//! `tunnel` 是 R2（yamux）/R3（iroh）的契约占位：token 可解析（配置合法），
//! 但 `init` 显式 bail 拒绝——不虚报支持；懒读路径 `reach_mode` warn 回退
//! Published。
//!
//! 并发约束：OnceLock 读多写一（init/懒兜底各一次），无锁序问题。

use serde::{Deserialize, Serialize};

/// env 覆盖键（config `deploy_host.reach` 之上的最高优先级）。
pub const REACH_ENV: &str = "RCODER_DEPLOY_HOST_REACH";

/// 配置面设置（三级解析的输入；类型不门控——config 面无 deploy-host feature
/// 也要能反序列化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReachSetting {
    /// 按生效 socket 特征 auto 检测
    #[default]
    Auto,
    /// 容器 IPv4 直拨（零端口发布）
    Direct,
    /// 端口发布到宿主机（既有行为）
    Published,
    /// R2/R3 隧道占位：可解析、init 拒绝
    Tunnel,
}

impl ReachSetting {
    /// 宽容解析：容忍大小写与首尾空白；未知 token → None。
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "direct" => Some(Self::Direct),
            "published" => Some(Self::Published),
            "tunnel" => Some(Self::Tunnel),
            _ => None,
        }
    }

    /// 规范 token（serde wire 形式同源小写）。
    pub fn as_token(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Direct => "direct",
            Self::Published => "published",
            Self::Tunnel => "tunnel",
        }
    }
}

// === 以下为运行时模式槽（deploy-host feature 门控；默认 feature 下仅类型可用） ===

/// Docker socket 默认探测路径。
#[cfg(feature = "deploy-host")]
pub const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";

#[cfg(feature = "deploy-host")]
use std::path::{Path, PathBuf};

#[cfg(feature = "deploy-host")]
use std::sync::OnceLock;

/// 进程级模式槽（创建容器前定一次，之后不可变）。
#[cfg(feature = "deploy-host")]
static SLOT: OnceLock<ReachMode> = OnceLock::new();

/// 生效的运行时模式（tunnel 不在其中——未实现即不可生效）。
#[cfg(feature = "deploy-host")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachMode {
    /// 容器真实 IPv4 直拨，容器端口原值即拨号端口
    Direct,
    /// 端口发布到宿主机，经端口映射表拨 loopback
    Published,
}

#[cfg(feature = "deploy-host")]
impl ReachMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Published => "published",
        }
    }
}

/// 生效 Docker socket 路径：DOCKER_HOST `unix://`/`unix:` 剥前缀
/// > DOCKER_SOCKET_PATH > `/var/run/docker.sock`。空串视为未设。
#[cfg(feature = "deploy-host")]
pub fn effective_socket_path() -> PathBuf {
    if let Ok(value) = std::env::var("DOCKER_HOST") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            for prefix in ["unix://", "unix:"] {
                if let Some(stripped) = trimmed.strip_prefix(prefix) {
                    return PathBuf::from(stripped);
                }
            }
            // 非 unix scheme（如 tcp:// 远端 daemon）：不作为本地 socket 路径，继续探测
        }
    }
    if let Ok(value) = std::env::var("DOCKER_SOCKET_PATH") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from(DEFAULT_DOCKER_SOCKET)
}

/// symlink 解一层（OrbStack 把 `/var/run/docker.sock` 链到
/// `~/.orbstack/run/docker.sock`——特征在真实目标里）。相对目标按链接父目录
/// join；非链接/失败返回原路径。
#[cfg(feature = "deploy-host")]
fn resolve_symlink_one_level(path: &Path) -> PathBuf {
    match std::fs::read_link(path) {
        Ok(target) => match path.parent() {
            Some(parent) if !target.is_absolute() => parent.join(&target),
            _ => target,
        },
        Err(_) => path.to_path_buf(),
    }
}

/// 纯函数：按已解析 socket 路径与平台判定默认模式。
///
/// 优先序固定：`.orbstack`（OrbStack 经 symlink 解出）> Docker Desktop 特征
/// > 原生 Linux > 未知安全默认。
#[cfg(feature = "deploy-host")]
pub fn classify_resolved_socket(resolved: &str, on_linux: bool) -> ReachMode {
    if resolved.contains(".orbstack") {
        ReachMode::Direct
    } else if resolved.contains("com.docker.docker") || resolved.contains("docker.raw") {
        ReachMode::Published
    } else if on_linux {
        ReachMode::Direct
    } else {
        ReachMode::Published
    }
}

/// 设置 → 生效模式。tunnel 显式拒绝（不虚报支持）。
#[cfg(feature = "deploy-host")]
pub fn resolve_mode(setting: ReachSetting) -> anyhow::Result<ReachMode> {
    match setting {
        ReachSetting::Direct => Ok(ReachMode::Direct),
        ReachSetting::Published => Ok(ReachMode::Published),
        ReachSetting::Auto => {
            let socket = resolve_symlink_one_level(&effective_socket_path());
            Ok(classify_resolved_socket(
                &socket.to_string_lossy(),
                cfg!(target_os = "linux"),
            ))
        }
        ReachSetting::Tunnel => anyhow::bail!(
            "deploy-host reach=tunnel 尚未实现（R2 yamux / R3 iroh 契约占位）；请改用 direct/published/auto"
        ),
    }
}

/// 启动时调用一次（容器创建前）。幂等：槽首写定终值，重复调用返回既有模式
/// 不覆盖；resolve 失败（tunnel）不污染槽。结果 eprintln+tracing 双明示
/// （console 在 tracing 未就绪时也可见，对齐 FeatureFlags::init 先例）。
#[cfg(feature = "deploy-host")]
pub fn init(setting: ReachSetting) -> anyhow::Result<ReachMode> {
    let mode = resolve_mode(setting)?;
    let mode = *SLOT.get_or_init(|| mode);
    let socket = resolve_symlink_one_level(&effective_socket_path());
    eprintln!(
        "🎯 [DEPLOY_HOST_REACH] mode={} setting={} socket={}",
        mode.as_str(),
        setting.as_token(),
        socket.display()
    );
    tracing::info!(
        mode = mode.as_str(),
        setting = setting.as_token(),
        socket = %socket.display(),
        "deploy-host reach mode resolved"
    );
    Ok(mode)
}

/// 读模式槽；未 init 时懒兜底（env → auto），保证永远可用（测试/忘记 init）。
/// env 非法 token warn 回退 auto；tunnel warn 回退 Published（读路径不 bail）。
/// 懒解析结果同样写入槽（进程级一次语义）。
#[cfg(feature = "deploy-host")]
pub fn reach_mode() -> ReachMode {
    if let Some(mode) = SLOT.get() {
        return *mode;
    }
    let setting = match std::env::var(REACH_ENV) {
        Ok(value) => match ReachSetting::from_token(&value) {
            Some(setting) => Some(setting),
            None if value.trim().is_empty() => None,
            None => {
                tracing::warn!(env = REACH_ENV, raw = %value, "invalid reach token; falling back to auto");
                None
            }
        },
        Err(_) => None,
    };
    let setting = setting.unwrap_or_default();
    let mode = match resolve_mode(setting) {
        Ok(mode) => mode,
        Err(error) => {
            tracing::warn!(%error, "reach mode resolve failed on lazy path; falling back to published");
            ReachMode::Published
        }
    };
    *SLOT.get_or_init(|| mode)
}

/// 便捷判定：当前是否 Direct（零端口发布 + IP 直拨）。
#[cfg(feature = "deploy-host")]
pub fn is_direct() -> bool {
    reach_mode() == ReachMode::Direct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_setting_serde_roundtrip_all_variants() {
        for token in ["auto", "direct", "published", "tunnel"] {
            let setting: ReachSetting = serde_json::from_str(&format!("\"{token}\""))
                .unwrap_or_else(|error| panic!("deserialize {token}: {error}"));
            let wire = serde_json::to_string(&setting).unwrap();
            assert_eq!(wire, format!("\"{token}\""));
            assert_eq!(setting.as_token(), token);
        }
    }

    #[test]
    fn reach_setting_default_is_auto() {
        assert_eq!(ReachSetting::default(), ReachSetting::Auto);
    }

    #[test]
    fn reach_setting_from_token_tolerates_case_and_whitespace() {
        assert_eq!(
            ReachSetting::from_token(" Direct "),
            Some(ReachSetting::Direct)
        );
        assert_eq!(
            ReachSetting::from_token("PUBLISHED"),
            Some(ReachSetting::Published)
        );
        assert_eq!(
            ReachSetting::from_token("\tauto\n"),
            Some(ReachSetting::Auto)
        );
        assert_eq!(
            ReachSetting::from_token("tunnel"),
            Some(ReachSetting::Tunnel)
        );
    }

    #[test]
    fn reach_setting_from_token_rejects_unknown() {
        assert_eq!(ReachSetting::from_token(""), None);
        assert_eq!(ReachSetting::from_token("orb"), None);
        assert_eq!(ReachSetting::from_token("direct,published"), None);
    }

    #[cfg(feature = "deploy-host")]
    mod gated {
        // 测试模块 env 变异豁免（edition 2024 set_var/remove_var 为 unsafe；
        // 与 loader.rs/app-cli 同款——仅测试代码、仅 env 变异）
        #![allow(unsafe_code)]

        use super::*;

        #[test]
        fn classify_prefers_orbstack_over_docker_desktop() {
            // OrbStack 特征必须最先判：路径同时含两标记时 OrbStack 语义优先
            let both = "/Users/x/.orbstack/run/com.docker.docker.sock";
            assert_eq!(classify_resolved_socket(both, false), ReachMode::Direct);
            assert_eq!(classify_resolved_socket(both, true), ReachMode::Direct);
        }

        #[test]
        fn classify_docker_desktop_markers_map_to_published() {
            for marker in ["com.docker.docker", "docker.raw"] {
                assert_eq!(
                    classify_resolved_socket(&format!("/var/run/{marker}/docker.sock"), false),
                    ReachMode::Published
                );
            }
        }

        #[test]
        fn classify_linux_maps_to_direct() {
            assert_eq!(
                classify_resolved_socket("/var/run/docker.sock", true),
                ReachMode::Direct
            );
        }

        #[test]
        fn classify_unknown_maps_to_published_safe_default() {
            assert_eq!(
                classify_resolved_socket("/var/run/docker.sock", false),
                ReachMode::Published
            );
            assert_eq!(classify_resolved_socket("", true), ReachMode::Direct);
            assert_eq!(classify_resolved_socket("", false), ReachMode::Published);
        }

        #[test]
        fn resolve_mode_passthrough_direct_and_published() {
            assert_eq!(
                resolve_mode(ReachSetting::Direct).unwrap(),
                ReachMode::Direct
            );
            assert_eq!(
                resolve_mode(ReachSetting::Published).unwrap(),
                ReachMode::Published
            );
        }

        #[test]
        fn resolve_mode_rejects_tunnel() {
            let error = resolve_mode(ReachSetting::Tunnel).expect_err("tunnel must bail");
            assert!(
                error.to_string().contains("tunnel"),
                "error should name tunnel: {error}"
            );
        }

        #[test]
        fn effective_socket_path_prefers_docker_host_unix_scheme() {
            unsafe {
                std::env::set_var("DOCKER_HOST", "unix:///custom/orb.sock");
                std::env::set_var("DOCKER_SOCKET_PATH", "/ignored.sock");
            }
            assert_eq!(effective_socket_path(), PathBuf::from("/custom/orb.sock"));

            unsafe { std::env::set_var("DOCKER_HOST", "unix:/plain-prefix.sock") };
            assert_eq!(effective_socket_path(), PathBuf::from("/plain-prefix.sock"));

            unsafe { std::env::set_var("DOCKER_HOST", "tcp://1.2.3.4:2375") };
            assert_eq!(effective_socket_path(), PathBuf::from("/ignored.sock"));

            unsafe {
                std::env::remove_var("DOCKER_HOST");
                std::env::set_var("DOCKER_SOCKET_PATH", "  ");
            }
            assert_eq!(
                effective_socket_path(),
                PathBuf::from(DEFAULT_DOCKER_SOCKET)
            );
        }

        #[test]
        fn symlink_resolves_one_level_with_relative_target() {
            let dir = tempfile::tempdir().expect("tempdir");
            let link = dir.path().join("docker.sock");
            // 相对目标：链接父目录下的 nested/real.sock
            std::fs::create_dir(dir.path().join("nested")).expect("mkdir");
            let real = dir.path().join("nested").join("real.sock");
            std::fs::write(&real, b"").expect("touch real sock");
            std::os::unix::fs::symlink("nested/real.sock", &link).expect("symlink");
            assert_eq!(
                resolve_symlink_one_level(&link),
                dir.path().join("nested").join("real.sock")
            );
            // 非链接路径原样返回
            assert_eq!(resolve_symlink_one_level(&real), real);
        }

        #[test]
        fn auto_classification_follows_effective_socket_env() {
            unsafe {
                std::env::remove_var("DOCKER_HOST");
                std::env::set_var("DOCKER_SOCKET_PATH", "/opt/x.orbstack/run/docker.sock");
            }
            assert_eq!(resolve_mode(ReachSetting::Auto).unwrap(), ReachMode::Direct);

            unsafe {
                std::env::set_var("DOCKER_SOCKET_PATH", "/opt/com.docker.docker/sock");
            }
            assert_eq!(
                resolve_mode(ReachSetting::Auto).unwrap(),
                ReachMode::Published
            );
        }

        // 以下两个用例写模式槽（nextest 进程隔离保证各自从空槽起步）
        #[test]
        fn reach_mode_lazy_reads_env_when_uninitialized() {
            unsafe {
                std::env::remove_var("DOCKER_HOST");
                std::env::remove_var("DOCKER_SOCKET_PATH");
                std::env::set_var(REACH_ENV, "direct");
            }
            assert_eq!(reach_mode(), ReachMode::Direct);
            assert!(is_direct());
        }

        #[test]
        fn reach_mode_lazy_invalid_env_falls_back_to_auto() {
            unsafe {
                std::env::remove_var("DOCKER_HOST");
                std::env::set_var(REACH_ENV, "bogus");
                std::env::set_var("DOCKER_SOCKET_PATH", "/tmp/x.orbstack/y.sock");
            }
            // 非法 token → auto → socket 特征 → Direct（未 panic、未虚报）
            assert_eq!(reach_mode(), ReachMode::Direct);
        }

        #[test]
        fn init_is_idempotent_first_write_wins() {
            unsafe {
                std::env::remove_var(REACH_ENV);
                std::env::set_var(REACH_ENV, "published");
            }
            // init 显式 Direct 覆盖 env；随后 env/再次 init 均不再改变槽
            assert_eq!(init(ReachSetting::Direct).unwrap(), ReachMode::Direct);
            assert_eq!(init(ReachSetting::Published).unwrap(), ReachMode::Direct);
            unsafe { std::env::set_var(REACH_ENV, "published") };
            assert_eq!(reach_mode(), ReachMode::Direct);
        }
    }
}
