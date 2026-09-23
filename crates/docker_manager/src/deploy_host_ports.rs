//! deploy-host Docker 容器寻址登记（Phase 4 引入；Direct 模式 R1 演进）。
//!
//! 宿主机形态下 agent/UserApp 容器按 Reach 模式登记到
//! [`shared_types::published`] 注册表，rcoder→容器全部拨号经注册表解析：
//!
//! - **Published**（既有行为）：容器端口发布到宿主机（Docker 自动分配，
//!   host_port 留空），创建后从 inspect `NetworkSettings.Ports` 读回实际映射，
//!   拨 `127.0.0.1:{host_port}`。Docker Desktop（macOS）容器网段对宿主机
//!   不可路由，发布端口是唯一可达路径。
//! - **Direct**（R1）：不发布任何端口，登记 inspect 容器真实 IPv4，拨
//!   `{container_ip}:{容器端口原值}`。OrbStack/原生 Linux 容器网段从宿主机
//!   可路由，零端口发布即达（绝不用 `容器名.orb.local`——OrbStack fake-IP
//!   198.18/15 恒超时）。
//!
//! 两种模式经统一入口 [`register_reach_from_inspect`] 登记，创建链与重启/
//! 扩缩刷新链共用。端口清单与 K8s `agent_service_ports`（k8s_service.rs）
//! 对称：同一组容器内端口在两种形态下都需被外部（rcoder 进程）触达。
use std::collections::HashMap;
use std::net::IpAddr;

use tracing::{error, info, warn};

use crate::DockerResult;
use shared_types::ServiceType;

/// gRPC（chat/SSE/状态查询）
pub const GRPC_PORT: u16 = shared_types::constants::GRPC_DEFAULT_PORT;
/// agent HTTP（健康/文件路由 merge 面）
pub const HTTP_PORT: u16 = shared_types::constants::HTTP_DEFAULT_PORT;
/// noVNC 数据面
pub const NOVNC_PORT: u16 = 6080;
/// ttyd Web 终端
pub const WS_TERMINAL_PORT: u16 = 17681;
/// 容器内 file-server（UserApp workspace build）
pub const FILE_SERVER_PORT: u16 = shared_types::AGENT_FILE_SERVER_PORT;
/// dbx Web GUI
pub const DBX_PORT: u16 = 4224;
/// audio WebSocket（computer 数据面）
pub const AUDIO_WS_PORT: u16 = 6089;
/// audio HTTP（computer 数据面）
pub const AUDIO_HTTP_PORT: u16 = 6090;
/// runtime ttyd 本体端口（dev_terminal runtime 会话直拨）
pub const TTYD_PORT: u16 = 7681;
/// app-cli 管理面（UserappBuilder 族）
pub const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;
/// UserApp 应用入口（UserappBuilder 族）
pub const APP_ENTRY_PORT: u16 = shared_types::APP_ENTRY_PORT;
/// IME 输入法服务（UserappBuilder 族）
pub const IME_PORT: u16 = 6091;

/// 按服务类型返回需发布的容器端口清单（容器内端口号；宿主端口由 Docker 分配）。
pub fn published_ports_for(service_type: &ServiceType) -> Vec<u16> {
    let mut ports = vec![
        HTTP_PORT,
        GRPC_PORT,
        NOVNC_PORT,
        WS_TERMINAL_PORT,
        FILE_SERVER_PORT,
        DBX_PORT,
        AUDIO_WS_PORT,
        AUDIO_HTTP_PORT,
    ];
    match service_type {
        ServiceType::UserappBuilder => {
            ports.extend_from_slice(&[APP_CLI_ADMIN_PORT, APP_ENTRY_PORT, IME_PORT]);
        }
        // computer 族也拨 IME（ime.rs 经 vnc_backends 键 + IME_PORT）与
        // runtime ttyd（7681，dev_terminal runtime 会话）
        ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
            ports.extend_from_slice(&[IME_PORT, TTYD_PORT]);
        }
        _ => {}
    }
    ports
}

/// 当前是否 Direct 模式（读进程级模式槽）。非 deploy-host 编译恒 false。
pub fn is_direct_reach() -> bool {
    #[cfg(feature = "deploy-host")]
    {
        shared_types::deploy_host_reach::is_direct()
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        false
    }
}

/// deploy-host Direct 运行形态抑制容器创建路径的全部端口发布
/// （agent builder auto_port_binding + app TCP/Http 双循环）。
/// 编译/运行非 deploy-host 恒 false——容器形态发布行为零变化（红线：
/// feature + 运行时双层判定）。
pub fn suppress_port_publishing() -> bool {
    #[cfg(feature = "deploy-host")]
    {
        shared_types::is_deploy_host() && is_direct_reach()
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        false
    }
}

/// 从 inspect `NetworkSettings.Ports` 解析容器端口→宿主端口映射。
///
/// 键形如 `"8086/tcp"`，值为绑定列表（Docker 自动分配时 host_port 由 daemon
/// 填充实际宿主端口）。读不到映射的端口跳过，拨号端明确报地址未就绪。
fn parse_published_map(
    network_ports: &Option<HashMap<String, Option<Vec<bollard::models::PortBinding>>>>,
) -> HashMap<u16, u16> {
    let Some(ports) = network_ports else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for (container_port_spec, bindings) in ports {
        let Some((raw_port, "tcp")) = container_port_spec.split_once('/') else {
            continue;
        };
        let Ok(container_port) = raw_port.parse::<u16>() else {
            continue;
        };
        let Some(host_port) = bindings
            .iter()
            .flatten()
            .filter_map(|binding| binding.host_port.as_deref())
            .filter_map(|raw| raw.parse::<u16>().ok())
            .find(|port| *port != 0)
        else {
            continue;
        };
        map.insert(container_port, host_port);
    }
    map
}

/// A running Published builder is usable only when every port from the same
/// policy as initial creation has a concrete host binding.
pub fn missing_builder_ports(inspect: &bollard::models::ContainerInspectResponse) -> Vec<u16> {
    missing_published_ports(inspect, &published_ports_for(&ServiceType::UserappBuilder))
}

pub fn missing_published_ports(
    inspect: &bollard::models::ContainerInspectResponse,
    required: &[u16],
) -> Vec<u16> {
    let ports = inspect
        .network_settings
        .as_ref()
        .and_then(|ns| ns.ports.clone());
    let published = parse_published_map(&ports);
    required
        .iter()
        .copied()
        .filter(|port| !published.contains_key(port))
        .collect()
}

/// 统一登记入口：按当前 Reach 模式把容器 inspect 登记到注册表。
///
/// **Published**：解析 `NetworkSettings.Ports` 映射整表登记；**Direct**：
/// 取容器真实 IPv4（preferred 网卡优先，None=任意网卡）登记，IP 不可得
/// 时**不登记 + error 日志**（拨号端报地址未就绪，登记死 IP 比缺项更糟）。
/// 容器键与 Docker 真实名（inspect `.Name` 去斜杠）
/// 双键整条目替换（同名容器重建时旧映射不残留；Direct/Published 互替同理）。
pub fn register_reach_from_inspect(
    container_name: &str,
    preferred_network: Option<&str>,
    inspect: &bollard::models::ContainerInspectResponse,
) -> DockerResult<()> {
    // 双键注册：identifier（starter 的 container_id 键）+ Docker 真实容器名
    // （inspect .Name 去前导 '/'——get_agent_info 查询键）。两键指向同一端口表，
    // 查询侧无论用哪个身份都命中（2026-09-22 宿主机实测：单键不匹配导致健康
    // 检查回退 127.0.0.1:容器端口 60s 超时）。
    let docker_name = inspect
        .name
        .as_deref()
        .map(|name| name.trim_start_matches('/').to_owned())
        .filter(|name| *name != container_name);
    let identifier = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("identifier"))
        .filter(|value| {
            value.as_str() != container_name && docker_name.as_deref() != Some(value.as_str())
        })
        .cloned();
    let physical_uid = inspect.id.as_deref().ok_or_else(|| {
        crate::DockerError::ConnectionError("Container inspect has no physical ID".into())
    })?;
    let created_at = inspect
        .created
        .as_deref()
        .ok_or_else(|| {
            crate::DockerError::ConnectionError("Container inspect has no creation time".into())
        })?
        .parse::<chrono::DateTime<chrono::Utc>>()
        .map_err(|error| {
            crate::DockerError::ConnectionError(format!(
                "Invalid Docker container creation time: {error}"
            ))
        })?;
    let register = |name: &str, reach| {
        if !shared_types::published::register_physical_at(name, physical_uid, created_at, reach) {
            warn!(
                name,
                physical_uid, "Ignored stale Docker address observation"
            );
        }
    };

    #[cfg(feature = "deploy-host")]
    if shared_types::deploy_host_reach::is_direct() {
        let raw = crate::runtime::docker_runtime::extract_container_ip(inspect, preferred_network);
        return match raw.parse::<IpAddr>() {
            Ok(ip) if !ip.is_unspecified() => {
                register(
                    container_name,
                    shared_types::published::Reach::Direct { host: ip },
                );
                info!("[deploy-host] direct reach registered: container={container_name} ip={ip}");
                if let Some(docker_name) = docker_name.as_deref() {
                    register(
                        docker_name,
                        shared_types::published::Reach::Direct { host: ip },
                    );
                    info!("[deploy-host] direct reach registered: container={docker_name} ip={ip}");
                }
                if let Some(identifier) = identifier.as_deref() {
                    register(
                        identifier,
                        shared_types::published::Reach::Direct { host: ip },
                    );
                }
                Ok(())
            }
            Ok(_) | Err(_) => {
                shared_types::published::unregister_if_physical(container_name, physical_uid);
                if let Some(docker_name) = docker_name.as_deref() {
                    shared_types::published::unregister_if_physical(docker_name, physical_uid);
                }
                if let Some(identifier) = identifier.as_deref() {
                    shared_types::published::unregister_if_physical(identifier, physical_uid);
                }
                error!(
                    "[deploy-host] direct reach: container {container_name} ip unavailable \
                     (raw={raw:?}); clearing stale address"
                );
                Ok(())
            }
        };
    }

    // Published 分支（非 deploy-host 编译也走此路——调用方已按运行形态守卫）
    let network_ports = inspect
        .network_settings
        .as_ref()
        .and_then(|ns| ns.ports.clone());
    let map = parse_published_map(&network_ports);
    let registered = map.len();
    let summary = network_ports
        .as_ref()
        .map(|ports| ports.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    if let Some(docker_name) = docker_name.as_deref() {
        register(
            docker_name,
            shared_types::published::Reach::Published { ports: map.clone() },
        );
        info!(
            "[deploy-host] published ports registered: container={}, entries={} ({})",
            docker_name, registered, summary
        );
    }
    if let Some(identifier) = identifier.as_deref() {
        register(
            identifier,
            shared_types::published::Reach::Published { ports: map.clone() },
        );
    }
    register(
        container_name,
        shared_types::published::Reach::Published { ports: map },
    );
    info!(
        "[deploy-host] published ports registered: container={}, entries={} ({})",
        container_name, registered, summary
    );
    Ok(())
}
