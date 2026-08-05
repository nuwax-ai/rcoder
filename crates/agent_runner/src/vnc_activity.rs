//! VNC 桌面连接活跃度计数
//!
//! agent_runner 不在 VNC 数据流上 —— VNC 由 Pingora 直接代理到容器 noVNC 端口
//! (`shared_types::NOVNC_PORT` = 6080，见 rcoder-proxy `handle_vnc_upstream`)，故无法像
//! 终端 (`ws_terminal`，进程内 `AtomicUsize`) 那样在连接建立/断开时计数。
//!
//! 这里改读 `/proc/net/tcp`(`/proc/net/tcp6`) 统计 **本机 noVNC 端口上的 ESTABLISHED TCP
//! 连接数**：每个经 Pingora 转发进来的 VNC WebSocket 会话对应一条 TCP 连接；客户端断开
//! (关桌面 / 关浏览器标签) 连接即消失，计数归零。`/proc/net/tcp` 的解析交给成熟的
//! [`procfs`] crate，不手写十六进制解析。
//!
//! 供 `grpc::status::get_active_tasks_count` 折入 `active_tasks`，使 idle cleaner 把
//! 「桌面开着」的容器判为活跃而不被闲置回收 —— 桌面流量本身不刷新 `last_activity`、
//! 也不计入 agent active task，与终端当年接入 (`ws_terminal::active_terminal_count`)
//! 的动机完全一致。
//!
//! 平台：agent_runner 实际只在 Linux 容器内运行；非 Linux（macOS 开发编译）走 stub
//! 返回 0，`procfs` 依赖也只在 Linux target 引入（见 `Cargo.toml` 的
//! `[target.'cfg(target_os = "linux")'.dependencies]`），保持 Mac 开发编译干净。
//!
//! ⚙️ 开关（默认关）：由环境变量 `RCODER_VNC_ACTIVITY_PROCFS=1` 显式开启。**默认关闭时本函数
//! 直接返回 0、不读 /proc，对 `active_tasks` 零影响。** 默认关的原因：VNC 活跃监控目前由前端
//! `/computer/pod/keepalive`（`pollingWhenHidden:false`，tab 不可见时暂停）承担，后端 procfs 计数
//! 会给一条"无视 visibility"的 `last_activity` 刷新路径（30s status_keeper 见 WS 连接在就刷），
//! 反而让"tab 切后台"的容器收不回。故默认沿用 keepalive 方案，procfs 仅作可选开关保留。

// NOVNC_PORT 仅 Linux 的 is_vnc_client 用到（stub 分支与 cfg-agnostic 测试都不需要）；
// 不加 target gate 会在 macOS 上报 unused import。
#[cfg(target_os = "linux")]
use shared_types::NOVNC_PORT;
#[cfg(target_os = "linux")]
use std::sync::LazyLock;

/// procfs VNC 计数开关：仅当 `RCODER_VNC_ACTIVITY_PROCFS=1`/`true` 时为 true，默认 false。
/// 见模块文档说明（默认关，避免破坏前端 keepalive 的 visibility 门控）。
#[cfg(target_os = "linux")]
static PROCFS_VNC_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    is_procfs_vnc_env_enabled(std::env::var("RCODER_VNC_ACTIVITY_PROCFS").ok().as_deref())
});

/// 解析开关环境变量值 → 是否启用。抽出纯函数便于单测。缺省/空/未识别 → false。
#[cfg(target_os = "linux")]
fn is_procfs_vnc_env_enabled(value: Option<&str>) -> bool {
    match value {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => false,
    }
}

/// 统计当前容器内 noVNC 端口 (6080) 上的 ESTABLISHED TCP 连接数。
///
/// 同时读 `/proc/net/tcp`(IPv4) 与 `/proc/net/tcp6`(IPv6)。任一文件读取失败
/// (文件缺失、权限不足) 该文件按 0 计，整体不报错 —— 保守视作"无 VNC 连接"，
/// 不阻塞闲置回收逻辑。
///
/// **默认关闭**（`RCODER_VNC_ACTIVITY_PROCFS` 未置 1）：直接返回 0，不读 /proc。
#[cfg(target_os = "linux")]
pub fn active_vnc_client_count() -> usize {
    if !*PROCFS_VNC_ENABLED {
        return 0; // 默认关：不读 /proc，active_tasks 不受 VNC 连接影响
    }
    use procfs::net::{tcp, tcp6};

    let mut count = 0;
    for res in [tcp(), tcp6()] {
        match res {
            Ok(entries) => {
                count += entries
                    .iter()
                    .filter(|e| is_vnc_client(e.local_address.port(), &e.state))
                    .count();
            }
            Err(e) => {
                tracing::debug!("read /proc/net/tcp for VNC count failed (treat as 0): {e}");
            }
        }
    }
    count
}

/// 非 Linux 平台 stub（agent_runner 实际只在 Linux 容器内运行；此分支仅为 Mac 开发编译占位）。
#[cfg(not(target_os = "linux"))]
pub fn active_vnc_client_count() -> usize {
    0
}

/// 一条 TCP 连接是否算"VNC 客户端在用"：本地端口 == noVNC(6080) 且状态为 ESTABLISHED。
///
/// 抽成纯函数便于单测（不依赖 /proc 读取）。LISTEN / TIME_WAIT / CLOSE_WAIT 等状态不算
/// （监听套接字、刚断开的残留连接都不是"客户端在线"）。`TcpState` 非 Copy，故按引用传入。
#[cfg(target_os = "linux")]
fn is_vnc_client(local_port: u16, state: &procfs::net::TcpState) -> bool {
    local_port == NOVNC_PORT && matches!(state, procfs::net::TcpState::Established)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::is_vnc_client;
    #[cfg(target_os = "linux")]
    use procfs::net::TcpState;

    /// 过滤逻辑：只数 noVNC 端口(6080) + ESTABLISHED。
    /// （仅 Linux 编译——TcpState 来自 procfs；Mac 开发编译跳过此测试。）
    #[cfg(target_os = "linux")]
    #[test]
    fn is_vnc_client_matches_only_6080_established() {
        // 端口 + 状态都满足才算
        assert!(is_vnc_client(6080, &TcpState::Established));
        // 端口对、但状态非 ESTABLISHED → 不算（监听/残留连接不算客户端在线）
        assert!(!is_vnc_client(6080, &TcpState::Listen));
        assert!(!is_vnc_client(6080, &TcpState::TimeWait));
        assert!(!is_vnc_client(6080, &TcpState::CloseWait));
        assert!(!is_vnc_client(6080, &TcpState::SynSent));
        // 状态对、但端口不对 → 不算（5900 是 websockify→Xvnc 内部连接，不数）
        assert!(!is_vnc_client(5900, &TcpState::Established));
        assert!(!is_vnc_client(0, &TcpState::Established));
    }

    /// 防回归：noVNC 前端端口必须是 6080，与 is_vnc_client / active_vnc_client_count 一致。
    #[test]
    fn novnc_port_is_6080() {
        assert_eq!(shared_types::NOVNC_PORT, 6080);
    }

    /// 开关 env 解析：默认关，仅 "1"/"true"(大小写无关) 开。
    /// （仅 Linux 编译——被测函数 cfg-gated；Mac 跳过。）
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_switch_default_off_only_explicit_true_enables() {
        use super::is_procfs_vnc_env_enabled;
        // 缺省 / 空 / 未识别 → 关
        assert!(!is_procfs_vnc_env_enabled(None));
        assert!(!is_procfs_vnc_env_enabled(Some("")));
        assert!(!is_procfs_vnc_env_enabled(Some("0")));
        assert!(!is_procfs_vnc_env_enabled(Some("false")));
        assert!(!is_procfs_vnc_env_enabled(Some("no")));
        assert!(!is_procfs_vnc_env_enabled(Some("random")));
        // 仅 1 / true(大小写无关) → 开
        assert!(is_procfs_vnc_env_enabled(Some("1")));
        assert!(is_procfs_vnc_env_enabled(Some("true")));
        assert!(is_procfs_vnc_env_enabled(Some("TRUE")));
        assert!(is_procfs_vnc_env_enabled(Some("True")));
    }
}
