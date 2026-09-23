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

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tracing::warn;

pub use crate::constants::ReachNotReady;

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
    /// Kubernetes NodePort is reached through a cluster node, which need not
    /// be reachable on the RCoder host's loopback interface.
    NodePort {
        host: IpAddr,
        ports: HashMap<u16, u16>,
    },
}

#[derive(Debug, Clone)]
struct ReachEntry {
    reach: Reach,
    physical_uid: Option<String>,
    physical_created_at: Option<DateTime<Utc>>,
}

static REGISTRY: LazyLock<DashMap<String, ReachEntry>> = LazyLock::new(DashMap::new);

/// 登记单个端口的发布映射（entry 合并，容器多端口可分次登记）。
///
/// 仅作用于 Docker Published 条目；Direct/NodePort 条目忽略并 warn。
pub fn register_port(container: &str, container_port: u16, host_port: u16) {
    match REGISTRY.entry(container.to_owned()) {
        Entry::Occupied(mut occupied) => match &mut occupied.get_mut().reach {
            Reach::Published { ports } => {
                ports.insert(container_port, host_port);
                occupied.get_mut().physical_uid = None;
                occupied.get_mut().physical_created_at = None;
            }
            Reach::Direct { .. } | Reach::NodePort { .. } => {
                warn!(
                    container,
                    "deploy-host: register_port on non-Docker entry ignored"
                );
            }
        },
        Entry::Vacant(vacant) => {
            vacant.insert(ReachEntry {
                reach: Reach::Published {
                    ports: HashMap::from([(container_port, host_port)]),
                },
                physical_uid: None,
                physical_created_at: None,
            });
        }
    }
}

/// 整表登记 Published 映射（替换该容器的全部端口映射；K8s nodePort 读回共用）。
pub fn register(container: &str, ports: HashMap<u16, u16>) {
    REGISTRY.insert(
        container.to_owned(),
        ReachEntry {
            reach: Reach::Published { ports },
            physical_uid: None,
            physical_created_at: None,
        },
    );
}

pub fn register_node_ports(container: &str, host: IpAddr, ports: HashMap<u16, u16>) {
    REGISTRY.insert(
        container.to_owned(),
        ReachEntry {
            reach: Reach::NodePort { host, ports },
            physical_uid: None,
            physical_created_at: None,
        },
    );
}

/// A host-side Kubernetes runtime can override the node address when NodePort
/// is not forwarded to localhost (for example, OrbStack). Existing localhost
/// deployments retain their previous behavior when the variable is absent.
pub fn k8s_node_port_host() -> Result<IpAddr, String> {
    match std::env::var("RCODER_K8S_NODE_IP") {
        Ok(raw) => {
            let host = raw
                .parse::<IpAddr>()
                .map_err(|error| format!("RCODER_K8S_NODE_IP must be an IP address: {error}"))?;
            if host.is_unspecified() {
                return Err("RCODER_K8S_NODE_IP must not be unspecified".into());
            }
            Ok(host)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEPLOY_HOST_DIAL_IP),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("RCODER_K8S_NODE_IP must be valid UTF-8".into())
        }
    }
}

pub fn register_physical(container: &str, physical_uid: &str, reach: Reach) {
    REGISTRY.insert(
        container.to_owned(),
        ReachEntry {
            reach,
            physical_uid: Some(physical_uid.to_owned()),
            physical_created_at: None,
        },
    );
}

/// Docker inspect results may complete out of order. A delayed observation of
/// the older physical container cannot replace the current container's route.
/// Returns false when the observation lost to a newer Docker creation time.
pub fn register_physical_at(
    container: &str,
    physical_uid: &str,
    created_at: DateTime<Utc>,
    reach: Reach,
) -> bool {
    let replacement = ReachEntry {
        reach,
        physical_uid: Some(physical_uid.to_owned()),
        physical_created_at: Some(created_at),
    };
    match REGISTRY.entry(container.to_owned()) {
        Entry::Occupied(mut entry) => {
            if entry.get().physical_uid.as_deref() != Some(physical_uid)
                && entry
                    .get()
                    .physical_created_at
                    .is_some_and(|current| created_at < current)
            {
                return false;
            }
            entry.insert(replacement);
        }
        Entry::Vacant(entry) => {
            entry.insert(replacement);
        }
    }
    true
}

/// 登记 Direct 直拨条目（整条目替换；容器重建/漂移后新 IP 覆盖旧值）。
pub fn register_direct(container: &str, host: IpAddr) {
    REGISTRY.insert(
        container.to_owned(),
        ReachEntry {
            reach: Reach::Direct { host },
            physical_uid: None,
            physical_created_at: None,
        },
    );
}

/// 注销容器（容器删除/Service 删除路径）。
pub fn unregister(container: &str) -> Option<Reach> {
    REGISTRY.remove(container).map(|(_, entry)| entry.reach)
}

/// A retiring container may clear only addresses it registered itself. A new
/// physical instance under the same logical name is never invalidated by a
/// delayed old-container callback.
pub fn unregister_if_physical(container: &str, physical_uid: &str) -> bool {
    match REGISTRY.entry(container.to_owned()) {
        Entry::Occupied(occupied)
            if occupied.get().physical_uid.as_deref() == Some(physical_uid) =>
        {
            occupied.remove();
            true
        }
        _ => false,
    }
}

/// 容器的某个容器端口对应的宿主机端口（仅 Published 且已映射时有值）。
pub fn host_port(container: &str, container_port: u16) -> Option<u16> {
    REGISTRY
        .get(container)
        .and_then(|entry| match &entry.value().reach {
            Reach::Direct { .. } => None,
            Reach::Published { ports } | Reach::NodePort { ports, .. } => {
                ports.get(&container_port).copied()
            }
        })
}

/// 解析容器端口的拨号地址。
///
/// - Direct：`{容器IP}:{容器端口原值}`
/// - Published：`127.0.0.1:{host_port}`
/// - NodePort：`{configured node IP}:{node_port}`
/// - 未注册 / Published 未映射：明确报告地址未就绪，绝不猜测本机端口。
pub fn resolve_published_addr(
    container: &str,
    container_port: u16,
) -> Result<SocketAddr, ReachNotReady> {
    if let Some(addr) = REGISTRY
        .get(container)
        .and_then(|entry| match &entry.value().reach {
            Reach::Direct { host } => Some(SocketAddr::new(*host, container_port)),
            Reach::Published { ports } => ports
                .get(&container_port)
                .copied()
                .map(|host_port| SocketAddr::new(DEPLOY_HOST_DIAL_IP, host_port)),
            Reach::NodePort { host, ports } => ports
                .get(&container_port)
                .copied()
                .map(|node_port| SocketAddr::new(*host, node_port)),
        })
    {
        return Ok(addr);
    }
    warn!(
        container,
        container_port, "deploy-host: no reachable address registered"
    );
    Err(ReachNotReady {
        container: container.to_owned(),
        port: container_port,
    })
}

/// 注册表快照（启动日志/验收比对用）。
pub fn snapshot() -> HashMap<String, Reach> {
    REGISTRY
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().reach.clone()))
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
            resolve_published_addr(&container, 50051).expect("published address"),
            SocketAddr::new(DEPLOY_HOST_DIAL_IP, 32199)
        );
        unregister(&container);
    }

    #[test]
    fn resolve_unregistered_reports_not_ready() {
        let container = unique_container("fallback");
        assert!(resolve_published_addr(&container, 50051).is_err());
    }

    #[test]
    fn node_port_uses_configured_node_ip_without_changing_docker_published() {
        let container = unique_container("node-port");
        let host: IpAddr = "192.168.139.2".parse().expect("test IP");
        register_node_ports(&container, host, HashMap::from([(8086, 31151)]));
        assert_eq!(
            resolve_published_addr(&container, 8086).expect("NodePort address"),
            SocketAddr::new(host, 31151)
        );
        unregister(&container);
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
            resolve_published_addr(&container, 8086).expect("direct address"),
            SocketAddr::new(ip, 8086)
        );
        assert_eq!(
            resolve_published_addr(&container, 50051).expect("direct address"),
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
            resolve_published_addr(&container, 8086).expect("direct address"),
            SocketAddr::new(ip, 8086)
        );
        register(&container, HashMap::from([(8086u16, 43211u16)]));
        assert_eq!(
            resolve_published_addr(&container, 8086).expect("published address"),
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
            resolve_published_addr(&container, 8086).expect("direct address"),
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

    #[test]
    fn retired_physical_uid_cannot_clear_replacement_route() {
        let container = unique_container("physical-fence");
        register_physical(
            &container,
            "old",
            Reach::Direct {
                host: "10.1.0.2".parse().unwrap(),
            },
        );
        register_physical(
            &container,
            "new",
            Reach::Direct {
                host: "10.1.0.3".parse().unwrap(),
            },
        );
        assert!(!unregister_if_physical(&container, "old"));
        assert_eq!(
            resolve_published_addr(&container, 8086)
                .unwrap()
                .ip()
                .to_string(),
            "10.1.0.3"
        );
        assert!(unregister_if_physical(&container, "new"));
        assert!(resolve_published_addr(&container, 8086).is_err());
    }
}
