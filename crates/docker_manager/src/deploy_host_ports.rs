//! deploy-host Docker 端口发布（Phase 4 引入；feature 门控）。
//!
//! 宿主机形态下 agent/UserApp 容器的端口发布到宿主机（Docker 自动分配，
//! host_port 留空），创建后从 inspect `NetworkSettings.Ports` 读回实际映射并
//! 登记 [`shared_types::published`] 注册表——rcoder→agent 的全部拨号经注册表
//! 解析 `127.0.0.1:{host_port}`（macOS 宿主机无法路由容器网段 IP）。
//!
//! 端口清单与 K8s `agent_service_ports`（k8s_service.rs）对称：同一组容器内
//! 端口在两种形态下都需被外部（rcoder 进程）触达。
use std::collections::HashMap;

use tracing::info;

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
    ];
    if matches!(service_type, ServiceType::UserappBuilder) {
        ports.extend_from_slice(&[APP_CLI_ADMIN_PORT, APP_ENTRY_PORT, IME_PORT]);
    }
    ports
}

/// 从容器 inspect 结果提取发布端口映射并登记注册表。
///
/// `NetworkSettings.Ports` 键形如 `"8086/tcp"`，值为绑定列表（Docker 自动分配时
/// host_port 由 daemon 填充实际宿主端口）。读不到映射的端口跳过（warn）——
/// 注册表缺项在拨号时回退 loopback:容器端口并 warn，可归因。
pub fn register_from_inspect(
    container_name: &str,
    network_ports: &Option<HashMap<String, Option<Vec<bollard::models::PortBinding>>>>,
) -> DockerResult<()> {
    let Some(ports) = network_ports else {
        return Ok(());
    };
    let mut map = HashMap::new();
    for (container_port_spec, bindings) in ports {
        let Some(container_port) = container_port_spec
            .split('/')
            .next()
            .and_then(|raw| raw.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(host_port) = bindings
            .iter()
            .flatten()
            .filter_map(|binding| binding.host_port.as_deref())
            .filter_map(|raw| raw.parse::<u16>().ok())
            .next()
        else {
            continue;
        };
        map.insert(container_port, host_port);
    }
    // 整表替换（非逐端口合并）：同名容器重建时旧映射不残留
    let registered = map.len();
    shared_types::published::register(container_name, map);
    info!(
        "[deploy-host] published ports registered: container={}, entries={} ({})",
        container_name,
        registered,
        ports.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}
