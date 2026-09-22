//! deploy-host Reach 寻址策略注册表（Phase 2 引入端口表，R1 演进为 Reach 枚举）。
//!
//! 宿主机形态两种寻址收敛到同一机制：**进程内注册表 container_name → [`Reach`]**。
//! - `Published { ports }`：容器端口发布到宿主机，拨号 `127.0.0.1:{host_port}`
//!   （Docker 自动分配读回；K8s nodePort 同机制）。macOS Docker Desktop 宿主机
//!   无法路由容器网段 IP，发布端口是唯一可达路径。
//! - `Direct { host }`：容器真实 IP 直拨，零端口发布，拨号
//!   `{容器IP}:{容器端口原值}`（OrbStack / Linux 原生等宿主机可路由容器网段的形态）。
//!
//! **不变量（R2 yamux / R3 iroh 隧道接缝）**：任何变体经
//! [`resolve_published_addr`] 物化为可拨 SocketAddr——允许"本地转发器"形态
//! （隧道模式下注册表登记本地转发器地址，resolve 返回转发端口，拨号面零改动）。
//!
//! 键即 container_name（与 funnel [`crate::build_backend_addr`] 的返回值同源，
//! 建连/清理路径天然一致）。启动时 rehydrate 回填存量容器/Service；容器停止/
//! 删除、Service 删除时 unregister。
//!
//! 并发约束（AGENTS.md §3）：DashMap entry API，不持 guard 跨 await。
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::LazyLock;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tracing::warn;

/// 宿主机形态 Published 模式拨号走 loopback（容器端口已发布到宿主）。
pub const DEPLOY_HOST_DIAL_IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// 容器寻址策略（注册表值类型）。
// Tunnel { forwarder } — R2(yamux)/R3(iroh) 预留：隧道模式下登记本地转发器地址，
// resolve 物化为转发端口，拨号面零改动（见模块 doc 不变量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// 容器真实 IP 直拨（零端口发布；宿主机可路由容器网段的形态）。
    Direct { host: IpAddr },
    /// 端口发布到宿主机（容器端口 → 宿主机端口映射）。
    Published { ports: HashMap<u16, u16> },
}

static REGISTRY: LazyLock<DashMap<String, Reach>> = LazyLock::new(DashMap::new);

/// 登记单个端口的发布映射（entry 合并，容器多端口可分次登记）。
///
/// 仅作用于 Published 条目；Direct 条目忽略并 warn（直拨条目无端口映射语义）。
pub fn register_port(container: &str, container_port: u16, host_port: u16) {
    match REGISTRY.entry(container.to_owned()) {
        Entry::Occupied(mut occupied) => match occupied.get_mut() {
            Reach::Published { ports } => {
                ports.insert(container_port, host_port);
            }
            Reach::Direct { .. } => {
                warn!(
                    container,
                    "deploy-host: register_port on Direct entry ignored"
                );
            }
        },
        Entry::Vacant(vacant) => {
            vacant.insert(Reach::Published {
                ports: HashMap::from([(container_port, host_port)]),
            });
        }
    }
}

/// 整表登记 Published 映射（替换该容器的全部端口映射；K8s nodePort 读回共用）。
pub fn register(container: &str, ports: HashMap<u16, u16>) {
    REGISTRY.insert(container.to_owned(), Reach::Published { ports });
}

/// 登记 Direct 直拨条目（整条目替换；容器重建/漂移后新 IP 覆盖旧值）。
pub fn register_direct(container: &str, host: IpAddr) {
    REGISTRY.insert(container.to_owned(), Reach::Direct { host });
}

/// 注销容器（容器删除/Service 删除路径）。
pub fn unregister(container: &str) -> Option<Reach> {
    REGISTRY.remove(container).map(|(_, reach)| reach)
}

/// 容器的某个容器端口对应的宿主机端口（仅 Published 且已映射时有值）。
pub fn host_port(container: &str, container_port: u16) -> Option<u16> {
    REGISTRY
        .get(container)
        .and_then(|reach| match reach.value() {
            Reach::Direct { .. } => None,
            Reach::Published { ports } => ports.get(&container_port).copied(),
        })
}

/// 解析容器端口的拨号地址。
///
/// - Direct：`{容器IP}:{容器端口原值}`
/// - Published：`127.0.0.1:{host_port}`
/// - 未注册 / Published 未映射：回退 `127.0.0.1:{container_port}` 并 warn——
///   deploy-host 编译下即缺陷，连接失败在运行期暴露且日志可归因。
pub fn resolve_published_addr(container: &str, container_port: u16) -> SocketAddr {
    if let Some(addr) = REGISTRY
        .get(container)
        .and_then(|reach| match reach.value() {
            Reach::Direct { host } => Some(SocketAddr::new(*host, container_port)),
            Reach::Published { ports } => ports
                .get(&container_port)
                .copied()
                .map(|host_port| SocketAddr::new(DEPLOY_HOST_DIAL_IP, host_port)),
        })
    {
        return addr;
    }
    warn!(
        container,
        container_port,
        "deploy-host: no reachable address registered; dialing loopback with container port"
    );
    SocketAddr::new(DEPLOY_HOST_DIAL_IP, container_port)
}

/// 注册表快照（启动日志/验收比对用）。
pub fn snapshot() -> HashMap<String, Reach> {
    REGISTRY
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_container(label: &str) -> String {
        format!("deploy-host-test-{label}-{}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn register_port_merges_into_existing_entry() {
        let container = unique_container("merge");
        register_port(&container, 8086, 32101);
        register_port(&container, 50051, 32102);
        register_port(&container, 8086, 32103); // 覆盖同容器端口
        assert_eq!(host_port(&container, 8086), Some(32103));
        assert_eq!(host_port(&container, 50051), Some(32102));
        assert_eq!(host_port(&container, 6080), None);
        unregister(&container);
    }

    #[test]
    fn resolve_returns_loopback_with_registered_port() {
        let container = unique_container("resolve");
        register_port(&container, 50051, 32199);
        assert_eq!(
            resolve_published_addr(&container, 50051),
            SocketAddr::new(DEPLOY_HOST_DIAL_IP, 32199)
        );
        unregister(&container);
    }

    #[test]
    fn resolve_unregistered_falls_back_to_container_port() {
        let container = unique_container("fallback");
        assert_eq!(
            resolve_published_addr(&container, 50051),
            SocketAddr::new(DEPLOY_HOST_DIAL_IP, 50051)
        );
    }

    #[test]
    fn register_replaces_whole_table() {
        let container = unique_container("replace");
        register(&container, HashMap::from([(8086u16, 41001u16)]));
        register(&container, HashMap::from([(50051u16, 41002u16)]));
        assert_eq!(host_port(&container, 8086), None);
        assert_eq!(host_port(&container, 50051), Some(41002));
        unregister(&container);
    }

    #[test]
    fn unregister_removes_and_returns_ports() {
        let container = unique_container("unregister");
        register_port(&container, 8086, 51001);
        let removed = unregister(&container).expect("registered entry");
        match removed {
            Reach::Published { ports } => assert_eq!(ports.get(&8086), Some(&51001)),
            other => panic!("expected Published, got {other:?}"),
        }
        assert_eq!(unregister(&container), None);
        assert_eq!(host_port(&container, 8086), None);
    }

    #[test]
    fn snapshot_reflects_registry_contents() {
        let container = unique_container("snapshot");
        register_port(&container, 8086, 61001);
        let snap = snapshot();
        match snap.get(&container).expect("snapshot entry") {
            Reach::Published { ports } => assert_eq!(ports.get(&8086), Some(&61001)),
            other => panic!("expected Published, got {other:?}"),
        }
        unregister(&container);
    }

    #[test]
    fn concurrent_register_and_resolve_are_consistent() {
        let container = unique_container("concurrent");
        let writers: Vec<_> = (0..8u16)
            .map(|i| {
                let name = container.clone();
                std::thread::spawn(move || {
                    register_port(&name, 20000 + i, 31000 + i);
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer thread");
        }
        for i in 0..8u16 {
            assert_eq!(host_port(&container, 20000 + i), Some(31000 + i));
        }
        unregister(&container);
    }

    #[test]
    fn register_direct_resolves_container_ip_with_container_port() {
        let container = unique_container("direct");
        let ip: IpAddr = "192.168.215.3".parse().expect("test ip");
        register_direct(&container, ip);
        assert_eq!(
            resolve_published_addr(&container, 8086),
            SocketAddr::new(ip, 8086)
        );
        assert_eq!(
            resolve_published_addr(&container, 50051),
            SocketAddr::new(ip, 50051)
        );
        unregister(&container);
    }

    #[test]
    fn register_direct_replaces_published_entry_and_vice_versa() {
        let container = unique_container("direct-replace");
        let ip: IpAddr = "172.20.0.9".parse().expect("test ip");
        register(&container, HashMap::from([(8086u16, 43210u16)]));
        register_direct(&container, ip);
        assert_eq!(
            resolve_published_addr(&container, 8086),
            SocketAddr::new(ip, 8086)
        );
        register(&container, HashMap::from([(8086u16, 43211u16)]));
        assert_eq!(
            resolve_published_addr(&container, 8086),
            SocketAddr::new(DEPLOY_HOST_DIAL_IP, 43211)
        );
        unregister(&container);
    }

    #[test]
    fn register_port_on_direct_entry_is_ignored() {
        let container = unique_container("direct-merge-guard");
        let ip: IpAddr = "10.7.0.5".parse().expect("test ip");
        register_direct(&container, ip);
        register_port(&container, 8086, 9999);
        assert_eq!(
            resolve_published_addr(&container, 8086),
            SocketAddr::new(ip, 8086)
        );
        unregister(&container);
    }

    #[test]
    fn direct_entry_host_port_is_none() {
        let container = unique_container("direct-host-port");
        let ip: IpAddr = "10.7.0.6".parse().expect("test ip");
        register_direct(&container, ip);
        assert_eq!(host_port(&container, 8086), None);
        unregister(&container);
    }

    #[test]
    fn unregister_returns_direct_reach() {
        let container = unique_container("direct-unregister");
        let ip: IpAddr = "10.7.0.7".parse().expect("test ip");
        register_direct(&container, ip);
        assert_eq!(
            unregister(&container),
            Some(Reach::Direct { host: ip }),
            "unregister 必须返回整条目（含 Direct）"
        );
    }
}
