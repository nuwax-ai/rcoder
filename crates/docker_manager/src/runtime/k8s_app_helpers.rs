//! UserApp Deployment 纯辅助函数(从 k8s_deployment.rs 拆出)。
//!
//! port-expose 注解编解码 + config_hash 注解 + probe 构建。create/query 共用。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{AppResourceRequirements, ContainerCreateParams, ExposeType};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{Probe, ResourceRequirements};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;

pub(crate) const PORT_EXPOSE_ANNOTATION: &str = "rcoder.io/port-expose";


/// ports → annotation（`rcoder.io/port-expose: "80:http,5432:tcp"`）；无端口返 None。
#[cfg(feature = "kubernetes")]
pub(crate) fn encode_port_expose_annotations(
    params: &ContainerCreateParams,
) -> Option<BTreeMap<String, String>> {
    let ports = params.ports.as_ref()?;
    if ports.is_empty() {
        return None;
    }
    // 按 port 排序后编码——避免调用方端口顺序差异触发 SSA 无谓 reconcile（顺序无关 → 字符串稳定）
    let mut entries: Vec<(u16, &ExposeType)> =
        ports.iter().map(|p| (p.port, &p.expose_type)).collect();
    entries.sort_by_key(|(port, _)| *port);
    let val = entries
        .iter()
        .map(|(port, et)| format!("{}:{}", port, expose_type_str(et)))
        .collect::<Vec<_>>()
        .join(",");
    let mut m = BTreeMap::new();
    m.insert(PORT_EXPOSE_ANNOTATION.to_string(), val);
    Some(m)
}

/// 解析 "port:type,..." → port→expose_type 映射（容错：非法条目跳过）。
#[cfg(feature = "kubernetes")]
pub(crate) fn parse_port_expose(s: &str) -> std::collections::HashMap<u16, ExposeType> {
    s.split(',')
        .filter_map(|entry| {
            let mut it = entry.split(':');
            let port: u16 = it.next()?.trim().parse().ok()?;
            let et = match it.next()?.trim() {
                "tcp" => ExposeType::Tcp,
                _ => ExposeType::Http,
            };
            Some((port, et))
        })
        .collect()
}

#[cfg(feature = "kubernetes")]
pub(crate) fn expose_type_str(e: &ExposeType) -> &'static str {
    match e {
        ExposeType::Http => "http",
        ExposeType::Tcp => "tcp",
    }
}

/// 计算 env+secrets 内容的 hash，注入 pod template annotation。
///
/// 作用：env/secrets 走 ConfigMap/Secret，改内容时 `env_from` 引用名不变 → Deployment spec
/// 不变 → 不触发 rollout → 新 env 到不了运行中 Pod。此 hash 进 pod template，内容变即
/// annotation 变 → spec 变 → 自动 rollout。DefaultHasher 跨进程确定（固定 key），故同
/// 内容多次 apply 的 hash 稳定，不会引发误 rollout。
#[cfg(feature = "kubernetes")]
pub(crate) fn config_hash_annotations(
    params: &container_runtime_api::ContainerCreateParams,
) -> BTreeMap<String, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for map in [params.env.as_ref(), params.secrets.as_ref()]
        .into_iter()
        .flatten()
    {
        let mut items: Vec<_> = map.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in items {
            k.hash(&mut h);
            v.hash(&mut h);
        }
    }
    let mut ann = BTreeMap::new();
    ann.insert(
        "rcoder.io/config-hash".to_string(),
        format!("{:016x}", h.finish()),
    );
    ann
}

/// 健康检查配置 → K8s Probe
#[cfg(feature = "kubernetes")]
pub(crate) fn build_probe(hc: &container_runtime_api::AppHealthCheck) -> Option<Probe> {
    use container_runtime_api::HealthCheckType;
    let init = hc.initial_delay_seconds.map(|s| s as i32);
    let period = hc.period_seconds.map(|s| s as i32);
    match hc.check_type {
        HealthCheckType::None | HealthCheckType::Exec => None,
        HealthCheckType::Http => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                    path: Some(hc.path.clone().unwrap_or_else(|| "/".to_string())),
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
        HealthCheckType::Tcp => {
            let port = hc.port.unwrap_or(80);
            Some(Probe {
                tcp_socket: Some(k8s_openapi::api::core::v1::TCPSocketAction {
                    port: IntOrString::Int(port as i32),
                    ..Default::default()
                }),
                initial_delay_seconds: init,
                period_seconds: period,
                ..Default::default()
            })
        }
    }
}

/// 构建 **requests/limits 解耦** 的 ResourceRequirements（agent 与 UserApp 两端共享策略）。
///
/// ⚙️ 策略：requests 设超小固定值（仅作 scheduler 调度保障量），limits 保留配置上限。
///   背景：常态空闲的 pod（agent-runner / UserApp）若 requests=limits（大值），scheduler
///   严格按 requests 预订 → 节点迅速占满 → Pod Pending。requests 小 → 支持超卖调度；
///   limits 大 → 单 Pod 突发不受限（最多 throttle / evict，不崩）。如需调整改下方固定值。
///
/// 各字段固定 requests（按资源可压缩性区分）：
/// - `cpu`（可压缩）：requests=`5m`。⚠️ cpu.shares 按 requests 算，5m 权重极低：节点 CPU
///   严重争抢时此 pod 会被深度 throttle（可能慢到 healthcheck 失败/启动超时）。集群 CPU
///   实测仅 5~7% 闲置，日常可轻松用超 requests；若遇 pod 启动超时或 healthcheck 失败，
///   先排查是否 CPU 饿死，必要时调大（如 50m）。
/// - `memory`（不可压缩）：requests=`64Mi`。pod 开了 swap，内存吃紧可换出不易 OOM；
///   运行时实际可用到 limits 上限（swap + limits 双兜底）。⚠️ 代价：节点内存严重紧张时，
///   低 requests + 高实际占用的 pod 可能被优先 evict；集群内存余量充足（实测 13~34%）
///   且有 swap，风险可控；若频繁被 evict 再调大。
/// - `ephemeral-storage`：requests=`512Mi`（overlay 实际写入少，数据在 PVC）。
///
/// 返回 `None`：三者皆空（无任何 limits 需求）。
#[cfg(feature = "kubernetes")]
pub(crate) fn build_decoupled_resources(
    cpu: Option<String>,
    memory: Option<String>,
    ephemeral: Option<String>,
) -> Option<ResourceRequirements> {
    let mut limits = BTreeMap::new();
    let mut requests = BTreeMap::new();
    if let Some(c) = cpu {
        limits.insert("cpu".to_string(), Quantity(c));
        requests.insert("cpu".to_string(), Quantity("5m".to_string()));
    }
    if let Some(m) = memory {
        limits.insert("memory".to_string(), Quantity(m));
        requests.insert("memory".to_string(), Quantity("64Mi".to_string()));
    }
    if let Some(es) = ephemeral {
        limits.insert("ephemeral-storage".to_string(), Quantity(es));
        requests.insert(
            "ephemeral-storage".to_string(),
            Quantity("512Mi".to_string()),
        );
    }
    if limits.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        requests: Some(requests),
        limits: Some(limits),
        ..Default::default()
    })
}

/// UserApp 资源需求 → ResourceRequirements（包一层共享 `build_decoupled_resources`）。
///
/// UserApp 的 `ephemeral_storage` 未指定时回退 `storage`（与 agent 侧
/// `ephemeral_storage_limit.or(storage_size)` 对称），二者同义（overlay 可写层配额）。
#[cfg(feature = "kubernetes")]
pub(crate) fn build_app_resource_requirements(
    req: &AppResourceRequirements,
) -> Option<ResourceRequirements> {
    let ephemeral = req.ephemeral_storage.clone().or_else(|| req.storage.clone());
    build_decoupled_resources(req.cpu.clone(), req.memory.clone(), ephemeral)
}
