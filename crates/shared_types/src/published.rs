//! deploy-host published-port 注册表（Phase 2 引入）。
//!
//! 两种宿主机模式收敛到同一寻址机制：**进程内注册表
//! container_name → {容器端口: 宿主机端口}**。Docker 侧在容器创建后从
//! inspect `NetworkSettings.Ports` 读回（Docker 自动分配）；K8s 侧在 Service
//! apply 后读回 server 分配的 nodePort。rcoder→agent 的全部拨号（gRPC 50051 /
//! HTTP 8086 / Pingora 数据面族）经 [`resolve_published_addr`] 得
//! `127.0.0.1:{host_port}`——macOS 宿主机无法路由容器网段 IP，也无法解析
//! 集群内 FQDN，发布端口是唯一可达路径。
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
use tracing::warn;

/// 宿主机形态下拨号一律走 loopback（agent 容器端口已发布到宿主）。
pub const DEPLOY_HOST_DIAL_IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

static REGISTRY: LazyLock<DashMap<String, HashMap<u16, u16>>> = LazyLock::new(DashMap::new);

/// 登记单个端口的发布映射（entry 合并，容器多端口可分次登记）。
pub fn register_port(container: &str, container_port: u16, host_port: u16) {
    REGISTRY
        .entry(container.to_owned())
        .or_default()
        .insert(container_port, host_port);
}

/// 整表登记（替换该容器的全部端口映射）。
pub fn register(container: &str, ports: HashMap<u16, u16>) {
    REGISTRY.insert(container.to_owned(), ports);
}

/// 注销容器（容器删除/Service 删除路径）。
pub fn unregister(container: &str) -> Option<HashMap<u16, u16>> {
    REGISTRY.remove(container).map(|(_, ports)| ports)
}

/// 容器的某个容器端口对应的宿主机端口。
pub fn host_port(container: &str, container_port: u16) -> Option<u16> {
    REGISTRY
        .get(container)
        .and_then(|ports| ports.get(&container_port).copied())
}

/// 解析容器端口的宿主机拨号地址（127.0.0.1:{host_port}）。
///
/// 未注册（Phase 2 早期：创建路径尚未接发布读回）时回退
/// `127.0.0.1:{container_port}` 并 warn——deploy-host 编译下端口未发布即缺陷，
/// 连接失败在运行期暴露且日志可归因。
pub fn resolve_published_addr(container: &str, container_port: u16) -> SocketAddr {
    match host_port(container, container_port) {
        Some(host_port) => SocketAddr::new(DEPLOY_HOST_DIAL_IP, host_port),
        None => {
            warn!(
                container,
                container_port,
                "deploy-host: no published port registered; dialing loopback with container port"
            );
            SocketAddr::new(DEPLOY_HOST_DIAL_IP, container_port)
        }
    }
}

/// 注册表快照（启动日志/验收比对用）。
pub fn snapshot() -> HashMap<String, HashMap<u16, u16>> {
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
        assert_eq!(removed.get(&8086), Some(&51001));
        assert_eq!(unregister(&container), None);
        assert_eq!(host_port(&container, 8086), None);
    }

    #[test]
    fn snapshot_reflects_registry_contents() {
        let container = unique_container("snapshot");
        register_port(&container, 8086, 61001);
        let snap = snapshot();
        assert_eq!(
            snap.get(&container).and_then(|ports| ports.get(&8086)),
            Some(&61001)
        );
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
}
