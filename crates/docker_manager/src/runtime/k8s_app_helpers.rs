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

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;
    use container_runtime_api::{AppHealthCheck, AppPortSpec, HealthCheckType};
    use shared_types::ServiceType;

    fn params_with_ports(ports: Vec<AppPortSpec>) -> ContainerCreateParams {
        ContainerCreateParams::builder()
            .project_id("test")
            .service_type(ServiceType::UserApp)
            .host_workspace_path("/tmp")
            .image_override("img")
            .ports(ports)
            .build()
    }

    fn params_with_env(env: std::collections::HashMap<String, String>) -> ContainerCreateParams {
        ContainerCreateParams::builder()
            .project_id("test")
            .service_type(ServiceType::UserApp)
            .host_workspace_path("/tmp")
            .image_override("img")
            .env(env)
            .build()
    }

    // ---- build_decoupled_resources：requests/limits 解耦策略 ----

    #[test]
    fn decoupled_resources_none_when_all_empty() {
        assert!(build_decoupled_resources(None, None, None).is_none());
    }

    #[test]
    fn decoupled_resources_cpu_only_sets_5m_request() {
        let rr = build_decoupled_resources(Some("2".into()), None, None).unwrap();
        let limits = rr.limits.unwrap();
        let requests = rr.requests.unwrap();
        assert_eq!(limits["cpu"].0.clone(), "2");
        assert_eq!(requests["cpu"].0.clone(), "5m");
        assert!(!limits.contains_key("memory"));
        assert!(!limits.contains_key("ephemeral-storage"));
    }

    #[test]
    fn decoupled_resources_all_three_decoupled_values() {
        let rr = build_decoupled_resources(
            Some("1".into()),
            Some("512Mi".into()),
            Some("1Gi".into()),
        )
        .unwrap();
        let limits = rr.limits.unwrap();
        let requests = rr.requests.unwrap();
        assert_eq!(limits["cpu"].0.clone(), "1");
        assert_eq!(requests["cpu"].0.clone(), "5m");
        assert_eq!(limits["memory"].0.clone(), "512Mi");
        assert_eq!(requests["memory"].0.clone(), "64Mi");
        assert_eq!(limits["ephemeral-storage"].0.clone(), "1Gi");
        assert_eq!(requests["ephemeral-storage"].0.clone(), "512Mi");
    }

    // ---- build_app_resource_requirements：ephemeral 回退 storage ----

    #[test]
    fn app_resources_ephemeral_falls_back_to_storage() {
        let req = AppResourceRequirements {
            storage: Some("5Gi".into()),
            ..Default::default()
        };
        let rr = build_app_resource_requirements(&req).unwrap();
        assert_eq!(
            rr.limits.unwrap()["ephemeral-storage"].0.clone(),
            "5Gi"
        );
    }

    #[test]
    fn app_resources_ephemeral_preferred_over_storage() {
        let req = AppResourceRequirements {
            storage: Some("5Gi".into()),
            ephemeral_storage: Some("1Gi".into()),
            ..Default::default()
        };
        let rr = build_app_resource_requirements(&req).unwrap();
        assert_eq!(
            rr.limits.unwrap()["ephemeral-storage"].0.clone(),
            "1Gi"
        );
    }

    #[test]
    fn app_resources_empty_returns_none() {
        assert!(build_app_resource_requirements(&AppResourceRequirements::default()).is_none());
    }

    // ---- parse_port_expose：编解码 + 容错 ----

    #[test]
    fn parse_port_expose_basic() {
        let m = parse_port_expose("80:http,5432:tcp");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&80), Some(&ExposeType::Http));
        assert_eq!(m.get(&5432), Some(&ExposeType::Tcp));
    }

    #[test]
    fn parse_port_expose_non_tcp_defaults_to_http() {
        // 非 "tcp" 一律当 Http（含未知字面量）—— 容错语义
        let m = parse_port_expose("8080:foo");
        assert_eq!(m.get(&8080), Some(&ExposeType::Http));
    }

    #[test]
    fn parse_port_expose_invalid_port_skipped() {
        let m = parse_port_expose("abc:http,443:tcp");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&443), Some(&ExposeType::Tcp));
    }

    // ---- encode_port_expose_annotations：排序 + round-trip ----

    #[test]
    fn encode_port_expose_sorted_and_roundtrips() {
        // 故意逆序输入：编码应按 port 升序（80 在前），保证 SSA 字符串稳定
        let ports = vec![
            AppPortSpec {
                name: "pg".into(),
                port: 5432,
                expose_type: ExposeType::Tcp,
                strip_prefix: None,
            },
            AppPortSpec {
                name: "http".into(),
                port: 80,
                expose_type: ExposeType::Http,
                strip_prefix: None,
            },
        ];
        let p = params_with_ports(ports);
        let ann = encode_port_expose_annotations(&p).unwrap();
        let encoded = ann.get(PORT_EXPOSE_ANNOTATION).unwrap();
        assert_eq!(encoded, "80:http,5432:tcp");
        // round-trip：编码后解析应还原
        let parsed = parse_port_expose(encoded);
        assert_eq!(parsed.get(&80), Some(&ExposeType::Http));
        assert_eq!(parsed.get(&5432), Some(&ExposeType::Tcp));
    }

    #[test]
    fn encode_port_expose_empty_returns_none() {
        let p = params_with_ports(vec![]);
        assert!(encode_port_expose_annotations(&p).is_none());
    }

    // ---- config_hash_annotations：稳定性（顺序无关 / 内容敏感）----

    #[test]
    fn config_hash_stable_regardless_of_entry_order() {
        let env1: std::collections::HashMap<_, _> = [
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]
        .into_iter()
        .collect();
        let env2: std::collections::HashMap<_, _> = [
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]
        .into_iter()
        .collect();
        let h1 = config_hash_annotations(&params_with_env(env1));
        let h2 = config_hash_annotations(&params_with_env(env2));
        assert_eq!(
            h1.get("rcoder.io/config-hash"),
            h2.get("rcoder.io/config-hash"),
            "同内容不同顺序必须 hash 一致（避免误 rollout）"
        );
    }

    #[test]
    fn config_hash_changes_on_content_change() {
        let env1: std::collections::HashMap<_, _> =
            [("A".to_string(), "1".to_string())].into_iter().collect();
        let env2: std::collections::HashMap<_, _> =
            [("A".to_string(), "2".to_string())].into_iter().collect();
        let h1 = config_hash_annotations(&params_with_env(env1));
        let h2 = config_hash_annotations(&params_with_env(env2));
        assert_ne!(
            h1.get("rcoder.io/config-hash"),
            h2.get("rcoder.io/config-hash")
        );
    }

    // ---- build_probe ----

    #[test]
    fn build_probe_none_and_exec_return_none() {
        let none_hc = AppHealthCheck {
            check_type: HealthCheckType::None,
            path: None,
            port: None,
            initial_delay_seconds: None,
            period_seconds: None,
        };
        assert!(build_probe(&none_hc).is_none());
        let exec_hc = AppHealthCheck {
            check_type: HealthCheckType::Exec,
            ..none_hc
        };
        assert!(build_probe(&exec_hc).is_none());
    }

    #[test]
    fn build_probe_http_defaults_port_and_path() {
        let hc = AppHealthCheck {
            check_type: HealthCheckType::Http,
            path: None,
            port: None,
            initial_delay_seconds: None,
            period_seconds: None,
        };
        let probe = build_probe(&hc).unwrap();
        let hg = probe.http_get.expect("http_get 必须设置");
        assert_eq!(hg.path, Some("/".to_string()));
        assert!(
            matches!(hg.port, IntOrString::Int(80)),
            "port 缺省应为 80"
        );
    }

    #[test]
    fn build_probe_tcp_sets_socket() {
        let hc = AppHealthCheck {
            check_type: HealthCheckType::Tcp,
            path: None,
            port: Some(9090),
            initial_delay_seconds: None,
            period_seconds: None,
        };
        let probe = build_probe(&hc).unwrap();
        assert!(probe.tcp_socket.is_some());
        assert!(probe.http_get.is_none());
    }
}
