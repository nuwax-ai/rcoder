//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, AppPortSpec, AppPortStatus, ContainerCreateParams,
    ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, ExposeType,
    RemovedContainerInfo, RuntimeContainerInfo,
};
use moka::future::Cache;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::DockerManager;

/// Docker runtime implementation wrapping DockerManager
pub struct DockerRuntime {
    pub(super) inner: Arc<DockerManager>,
    /// TTL cache for list_containers result (15 seconds)
    /// 列表缓存携带状态代次：命中时与 [`ContainerStateHandle::list_epoch`]
    /// 比对，变更（创建/删除/更新）后立即可见，不再单靠 TTL。
    list_cache: Cache<(), (u64, Vec<RuntimeContainerInfo>)>,
}

impl DockerRuntime {
    /// Create a new DockerRuntime wrapping the given DockerManager
    pub fn new(inner: Arc<DockerManager>) -> Self {
        Self {
            inner,
            list_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(15))
                .build(),
        }
    }
}

/// Userapp 容器/Deployment 命名（单一来源，与 K8s 侧 `KubernetesRuntime::app_deployment_name` 对称）。
///
/// 前缀取自 `ServiceType::Userapp::container_prefix()`，避免散落硬编码；改前缀只需改一处。
pub(super) fn app_deployment_name(app_id: &str) -> String {
    format!("{}-{app_id}", ServiceType::Userapp.container_prefix())
}

/// 容器 ports 元数据 label（update live 回退数据源；编码 "8080:http,5432:tcp"，
/// 与 K8s `rcoder.io/port-expose` 注解同构——Docker 侧 Http/Tcp 均无完整运行时
/// 落地可反推，见 create_deployment 内注释）。
pub(super) const APP_PORTS_LABEL: &str = "rcoder.io/app-ports";
/// 容器 command 元数据 label（JSON 数组；create 时用户显式设置才写入）。
pub(super) const APP_COMMAND_LABEL: &str = "rcoder.io/app-command";

/// ports → label 值（按端口排序编码，顺序无关 → 字符串稳定，避免无谓容器 diff）。
pub(super) fn encode_ports_label(ports: &[AppPortSpec]) -> String {
    let mut entries: Vec<(u16, &ExposeType)> =
        ports.iter().map(|p| (p.port, &p.expose_type)).collect();
    entries.sort_by_key(|(port, _)| *port);
    entries
        .iter()
        .map(|(port, et)| format!("{port}:{}", expose_type_str(et)))
        .collect::<Vec<_>>()
        .join(",")
}

/// label 值 → ports（容错：非法条目跳过；name 空串/strip_prefix None——Docker 单机
/// 模式这两项无运行时语义）。
pub(super) fn parse_ports_label(raw: &str) -> Vec<AppPortSpec> {
    raw.split(',')
        .filter_map(|entry| {
            let mut it = entry.split(':');
            let port: u16 = it.next()?.trim().parse().ok()?;
            let et = match it.next()?.trim() {
                "tcp" => ExposeType::Tcp,
                "http" => ExposeType::Http,
                _ => return None,
            };
            if it.next().is_some() {
                return None;
            }
            Some(AppPortSpec {
                name: String::new(),
                port,
                expose_type: et,
                strip_prefix: None,
            })
        })
        .collect()
}

pub(super) fn expose_type_str(e: &ExposeType) -> &'static str {
    match e {
        ExposeType::Http => "http",
        ExposeType::Tcp => "tcp",
    }
}

/// Docker `NanoCpus`（1 核 = 1e9）→ K8s Quantity 核数字符串（"1"/"0.5"）。
/// update 回退用：读回的值将再次下发为 K8s/Docker 资源限制。
pub(super) fn docker_cpus_to_quantity(nano_cpus: i64) -> String {
    let cores = nano_cpus as f64 / 1e9;
    if cores.fract() == 0.0 {
        format!("{}", cores as i64)
    } else {
        format!("{cores}")
    }
}

/// Docker 字节内存限制 → K8s Quantity 字符串（无损换算：优先整 Gi/Mi/Ki 档，非整档
/// 用更细档位精确表示——1.5Gi=1536Mi 而非缩水成 1Gi；非 Ki 整数倍的罕见值直接输出
/// 字节数，K8s Quantity 合法且无损）。
pub(super) fn docker_memory_to_quantity(bytes: i64) -> String {
    const KI: i64 = 1024;
    const MI: i64 = 1024 * 1024;
    const GI: i64 = 1024 * 1024 * 1024;
    if bytes >= GI && bytes % GI == 0 {
        format!("{}Gi", bytes / GI)
    } else if bytes >= MI && bytes % MI == 0 {
        format!("{}Mi", bytes / MI)
    } else if bytes >= KI && bytes % KI == 0 {
        format!("{}Ki", bytes / KI)
    } else {
        format!("{bytes}")
    }
}

/// 从容器 inspect 结果提取 IP：优先取 `preferred_network` 网卡，回退任意网卡。
///
/// Docker 容器可能同时连接多个网络（主网络 + 自定义），`networks.values().next()`
/// 会非确定性地取一个。优先按主网络名定位，确保拿到 Pingora backend 应指向的 IP。
pub(crate) fn extract_container_ip(
    inspect: &bollard::models::ContainerInspectResponse,
    preferred_network: Option<&str>,
) -> String {
    let Some(nets) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    else {
        return String::new();
    };
    if let Some(net) = preferred_network
        && let Some(entry) = nets.get(net)
        && let Some(ip) = entry.ip_address.as_ref()
        && !ip.is_empty()
    {
        return ip.clone();
    }
    nets.values()
        .next()
        .and_then(|e| e.ip_address.clone())
        .filter(|ip| !ip.is_empty())
        .unwrap_or_default()
}

/// 从容器 inspect 提取 TCP 端口状态（Docker port_bindings → host_port）。
///
/// Docker 仅对 TCP 端口做 port_bindings（create_deployment 时），HTTP 端口走 Pingora
/// 不做 binding，故此处只还原 TCP；name 用 `tcp-{port}`（Docker 无端口名概念，调用方
/// 按 port 而非 name 匹配 external_port）。
pub(super) fn extract_container_ports(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Vec<AppPortStatus> {
    let Some(ports_map) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.ports.as_ref())
    else {
        return vec![];
    };
    ports_map
        .iter()
        .filter_map(|(key, bindings)| {
            // key 形如 "80/tcp"
            let port: u16 = key.trim_end_matches("/tcp").parse().ok()?;
            let host_port = bindings
                .as_ref()
                .and_then(|b| b.first())
                .and_then(|pb| pb.host_port.as_deref())
                .and_then(|s| s.parse::<u16>().ok())?;
            Some(AppPortStatus {
                name: format!("tcp-{port}"),
                port,
                expose_type: ExposeType::Tcp,
                external_port: Some(host_port),
            })
        })
        .collect()
}

// 拆分（file-server 大文件范式）：`agent_runtime` `AgentContainerRuntime`
// trait impl（整块迁移，方法无可见性修饰）；`partial_creation` 半创建
// 收据记录（fn+impl 升 pub(super) 供 trait impl 块调用）；`container_query`
// fetch_containers + map_container_status（pub(super) 供 trait impl 块）。
// 命名/端口标签/quantity/IP 提取等自由函数留在 mod.rs：docker_app_create、
// docker_app_runtime、docker_readiness、docker_builder_control、
// agent_container_starter、deploy_host_ports 经 `docker_runtime::X` 路径
// 引用，可见性与路径均不变。块内 super:: 兄弟路径已改写为 crate::runtime::。

mod agent_runtime;
mod container_query;
mod partial_creation;
#[cfg(test)]
mod tests;

use container_query::*;
use partial_creation::*;
