//! Userapp Deployment 纯辅助函数(从 k8s_deployment.rs 拆出)。
//!
//! port-expose 注解编解码 + config_hash 注解 + probe 构建。create/query 共用。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{AppResourceRequirements, ContainerCreateParams, ExposeType};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{Probe, ResourceRequirements, TopologySpreadConstraint};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;

pub(crate) const PORT_EXPOSE_ANNOTATION: &str = "rcoder.io/port-expose";
/// 闲置回收开关注解（absent/"true"=可回收=免费默认；"false"=永不回收=付费/常驻）
pub(crate) const RECYCLE_ENABLED_ANNOTATION: &str = "rcoder.io/recycle-enabled";
/// 闲置回收阈值秒数注解（per-app 覆盖全局）
pub(crate) const IDLE_TIMEOUT_ANNOTATION: &str = "rcoder.io/idle-timeout-seconds";
pub(crate) const WAKE_ON_TRAFFIC_ANNOTATION: &str = "rcoder.io/wake-on-traffic";

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
pub(crate) fn config_hash_annotations(params: &ContainerCreateParams) -> BTreeMap<String, String> {
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

/// recycle 配置 → annotation：恒出 `rcoder.io/recycle-enabled`（None/true→"true"，false→"false"），
/// `idle_timeout_seconds` 仅 Some 时写入。供 [`merge_app_annotations`] 合并到 Deployment metadata。
#[cfg(feature = "kubernetes")]
pub(crate) fn encode_recycle_annotations(
    params: &ContainerCreateParams,
) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let enabled = params.recycle_enabled.unwrap_or(true);
    m.insert(RECYCLE_ENABLED_ANNOTATION.to_string(), enabled.to_string());
    if let Some(secs) = params.idle_timeout_seconds {
        m.insert(IDLE_TIMEOUT_ANNOTATION.to_string(), secs.to_string());
    }
    m
}

/// 合并 port-expose + recycle 注解为 Deployment metadata.annotations（SSA 单一事实源）。
/// encode_recycle_annotations 恒非空 → 结果恒 Some。
#[cfg(feature = "kubernetes")]
pub(crate) fn merge_app_annotations(
    params: &ContainerCreateParams,
) -> Option<BTreeMap<String, String>> {
    let mut ann = encode_port_expose_annotations(params).unwrap_or_default();
    ann.extend(encode_recycle_annotations(params));
    if ann.is_empty() { None } else { Some(ann) }
}

/// 健康检查配置 → K8s Probe
///
/// `is_liveness`:true 构建 liveness 探针(用 `liveness_path`,缺省回退 `path`);
/// false 构建 readiness 探针(用 `path`)。从而支持 liveness/readiness 不同路径的语义拆分。
#[cfg(feature = "kubernetes")]
pub(crate) fn build_probe(
    hc: &container_runtime_api::AppHealthCheck,
    is_liveness: bool,
) -> Option<Probe> {
    use container_runtime_api::HealthCheckType;
    let init = hc.initial_delay_seconds.map(|s| s as i32);
    let period = hc.period_seconds.map(|s| s as i32);
    match hc.check_type {
        HealthCheckType::None | HealthCheckType::Exec => None,
        HealthCheckType::Http => {
            let port = hc.port.unwrap_or(80);
            let path = if is_liveness {
                hc.liveness_path
                    .clone()
                    .or_else(|| hc.path.clone())
                    .unwrap_or_else(|| "/".to_string())
            } else {
                hc.path.clone().unwrap_or_else(|| "/".to_string())
            };
            Some(Probe {
                http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                    path: Some(path),
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

/// 构建 **requests/limits 解耦** 的 ResourceRequirements（agent 与 Userapp 两端共享策略）。
///
/// ⚙️ 策略：requests 设超小固定值（仅作 scheduler 调度保障量），limits 保留配置上限。
///   背景：常态空闲的 pod（agent-runner / Userapp）若 requests=limits（大值），scheduler
///   严格按 requests 预订 → 节点迅速占满 → Pod Pending。requests 小 → 支持超卖调度；
///   limits 大 → 单 Pod 突发不受限（最多 throttle / evict，不崩）。如需调整改下方固定值。
///
/// 各字段固定 requests（按资源可压缩性区分）：
/// - `cpu`（可压缩）：requests=`50m`。cpu.shares 按 requests 算，5m 权重极低，节点 CPU
///   严重争抢时此 pod 会被深度 throttle（曾观察到 healthcheck 失败/启动超时）。50m 是防
///   CPU 饿死的最低安全值（约为实际峰值 100~1200m 的零头，不影响"低 requests + 高 limits
///   超卖"的设计）。集群 CPU 实测仅 5~7% 闲置，日常可轻松用超 requests；若仍遇 pod 启动
///   超时或 healthcheck 失败，先排查是否 CPU 饿死，再考虑继续调大。
///   ⚠️ 50m 只缓解 throttle，**不解决调度均衡**：requests 仍很低，调度器仍按"节点很轻"
///   看待，会把 pod 全堆在最闲节点。跨节点均衡由 PodSpec 的 topologySpreadConstraints
///   保证（见 build_agent_pod_spec），不靠调 requests。
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
        requests.insert("cpu".to_string(), Quantity("50m".to_string()));
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

/// 构建"按节点(hostname)均衡"的 topologySpreadConstraint（软约束），agent-runner 与
/// Userapp 动态创建路径共用（统一均衡策略，改参数只改这一处）。
///
/// - `topologyKey=hostname`：每个 Node 一个 domain，pod 往节点间摊。
/// - `labelSelector=app.kubernetes.io/name=<label_value>`：分组统计 key——只统计同 label
///   的 pod（agent 侧与 `build_standard_labels`、app 侧与 `build_app_labels` 写入的 name
///   label 值一致），各组各管各的均衡，跨 STS/跨 Deployment 统计。
/// - `whenUnsatisfiable=ScheduleAnyway`：★绝不阻断调度。即使放到任何节点都违反 maxSkew
///   也照常调度（优先选 pod 最少的节点）。业务 pod（agent-runner/用户 app）绝不能因均衡
///   约束 Pending/创建失败（对比 DoNotSchedule：不满足就 Pending，会卡住业务，不采用）。
/// - `maxSkew=5`：⚠️ ScheduleAnyway 下 maxSkew 只在 DoNotSchedule 模式才作硬过滤阈值；
///   软约束模式下调度器永远优先 pod 最少的节点，与数值基本无关，取 5 仅表"允许一定倾斜"。
///
/// 根因分析与方案见 docs/agent-runner-scheduling-balance.md（requests 虚标致调度失衡）。
#[cfg(feature = "kubernetes")]
pub(crate) fn build_hostname_spread_constraint(label_value: &str) -> TopologySpreadConstraint {
    let mut match_labels = BTreeMap::new();
    match_labels.insert(
        "app.kubernetes.io/name".to_string(),
        label_value.to_string(),
    );
    TopologySpreadConstraint {
        max_skew: 5,
        topology_key: "kubernetes.io/hostname".to_string(),
        when_unsatisfiable: "ScheduleAnyway".to_string(),
        label_selector: Some(LabelSelector {
            match_labels: Some(match_labels),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Userapp 资源需求 → ResourceRequirements（包一层共享 `build_decoupled_resources`）。
///
/// Userapp 的 `ephemeral_storage` 未指定时回退 `storage`（与 agent 侧
/// `ephemeral_storage_limit.or(storage_size)` 对称），二者同义（overlay 可写层配额）。
#[cfg(feature = "kubernetes")]
pub(crate) fn build_app_resource_requirements(
    req: &AppResourceRequirements,
) -> Option<ResourceRequirements> {
    let ephemeral = req
        .ephemeral_storage
        .clone()
        .or_else(|| req.storage.clone());
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
            .service_type(ServiceType::Userapp)
            .image_override("img")
            .ports(ports)
            .build()
    }

    fn params_with_env(env: std::collections::HashMap<String, String>) -> ContainerCreateParams {
        ContainerCreateParams::builder()
            .project_id("test")
            .service_type(ServiceType::Userapp)
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
    fn decoupled_resources_cpu_only_sets_50m_request() {
        let rr = build_decoupled_resources(Some("2".into()), None, None).unwrap();
        let limits = rr.limits.unwrap();
        let requests = rr.requests.unwrap();
        assert_eq!(limits["cpu"].0.clone(), "2");
        assert_eq!(requests["cpu"].0.clone(), "50m");
        assert!(!limits.contains_key("memory"));
        assert!(!limits.contains_key("ephemeral-storage"));
    }

    #[test]
    fn decoupled_resources_all_three_decoupled_values() {
        let rr =
            build_decoupled_resources(Some("1".into()), Some("512Mi".into()), Some("1Gi".into()))
                .unwrap();
        let limits = rr.limits.unwrap();
        let requests = rr.requests.unwrap();
        assert_eq!(limits["cpu"].0.clone(), "1");
        assert_eq!(requests["cpu"].0.clone(), "50m");
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
        assert_eq!(rr.limits.unwrap()["ephemeral-storage"].0.clone(), "5Gi");
    }

    #[test]
    fn app_resources_ephemeral_preferred_over_storage() {
        let req = AppResourceRequirements {
            storage: Some("5Gi".into()),
            ephemeral_storage: Some("1Gi".into()),
            ..Default::default()
        };
        let rr = build_app_resource_requirements(&req).unwrap();
        assert_eq!(rr.limits.unwrap()["ephemeral-storage"].0.clone(), "1Gi");
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
            liveness_path: None,
            port: None,
            initial_delay_seconds: None,
            period_seconds: None,
        };
        assert!(build_probe(&none_hc, false).is_none());
        let exec_hc = AppHealthCheck {
            check_type: HealthCheckType::Exec,
            ..none_hc
        };
        assert!(build_probe(&exec_hc, true).is_none());
    }

    #[test]
    fn build_probe_http_defaults_port_and_path() {
        let hc = AppHealthCheck {
            check_type: HealthCheckType::Http,
            path: None,
            liveness_path: None,
            port: None,
            initial_delay_seconds: None,
            period_seconds: None,
        };
        let probe = build_probe(&hc, false).unwrap();
        let hg = probe.http_get.expect("http_get 必须设置");
        assert_eq!(hg.path, Some("/".to_string()));
        assert!(matches!(hg.port, IntOrString::Int(80)), "port 缺省应为 80");
    }

    #[test]
    fn build_probe_liveness_uses_liveness_path() {
        // liveness_path 设了 → liveness 用它;readiness 仍用 path。
        let hc = AppHealthCheck {
            check_type: HealthCheckType::Http,
            path: Some("/ready".to_string()),
            liveness_path: Some("/health".to_string()),
            port: Some(3010),
            initial_delay_seconds: None,
            period_seconds: None,
        };
        let live = build_probe(&hc, true).unwrap().http_get.unwrap();
        let ready = build_probe(&hc, false).unwrap().http_get.unwrap();
        assert_eq!(
            live.path,
            Some("/health".to_string()),
            "liveness 用 liveness_path"
        );
        assert_eq!(ready.path, Some("/ready".to_string()), "readiness 用 path");
    }

    #[test]
    fn build_probe_tcp_sets_socket() {
        let hc = AppHealthCheck {
            check_type: HealthCheckType::Tcp,
            path: None,
            liveness_path: None,
            port: Some(9090),
            initial_delay_seconds: None,
            period_seconds: None,
        };
        let probe = build_probe(&hc, false).unwrap();
        assert!(probe.tcp_socket.is_some());
        assert!(probe.http_get.is_none());
    }

    #[test]
    fn encode_recycle_annotations_roundtrip() {
        use container_runtime_api::ContainerCreateParams;

        // 付费 app:recycle=false + 自定义 idle 阈值
        let paid = ContainerCreateParams::builder()
            .recycle_enabled(false)
            .idle_timeout_seconds(86400)
            .build();
        let ann = encode_recycle_annotations(&paid);
        assert_eq!(
            ann.get(RECYCLE_ENABLED_ANNOTATION).map(String::as_str),
            Some("false")
        );
        assert_eq!(
            ann.get(IDLE_TIMEOUT_ANNOTATION).map(String::as_str),
            Some("86400")
        );

        // 免费默认:recycle=None → "true";无 idle 注解
        let free = ContainerCreateParams::builder().build();
        let ann2 = encode_recycle_annotations(&free);
        assert_eq!(
            ann2.get(RECYCLE_ENABLED_ANNOTATION).map(String::as_str),
            Some("true")
        );
        assert!(!ann2.contains_key(IDLE_TIMEOUT_ANNOTATION));

        // merge 恒出 recycle-enabled(即使无端口 → port-expose 为 None,merge 仍非空)
        let merged = merge_app_annotations(&free).expect("merge non-empty (recycle-enabled)");
        assert!(merged.contains_key(RECYCLE_ENABLED_ANNOTATION));
    }
}
