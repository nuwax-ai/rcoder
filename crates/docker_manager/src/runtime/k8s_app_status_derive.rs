//! K8s app 状态推导纯函数（自 k8s_app_query.rs 拆出；函数体原样搬迁）。
//! 零 self 依赖，全部可单测（13 个测试随迁）。

#![cfg(feature = "kubernetes")]

use std::collections::HashMap;

use container_runtime_api::{AppPortStatus, ExposeType};
use k8s_openapi::api::apps::v1::Deployment;

use super::k8s_app_helpers::{PORT_EXPOSE_ANNOTATION, parse_port_expose};

/// phase 推导（纯函数）。
///
/// replicas=0 → Stopped；容器启动失败 → Error（优先于 ready 判定，避免 CrashLoop 期间
/// 偶发 ready_replicas>0 被误报 Running）；就绪副本达标 → Running；否则 Starting。
pub(super) fn derive_phase(
    replicas: i32,
    ready_replicas: i32,
    error_message: &Option<String>,
) -> String {
    if replicas == 0 {
        "Stopped".to_string()
    } else if error_message.is_some() {
        "Error".to_string()
    } else if ready_replicas >= replicas && ready_replicas > 0 {
        "Running".to_string()
    } else {
        "Starting".to_string()
    }
}

/// 端口状态推导（纯函数）：从 Deployment spec container ports + annotation port-expose +
/// TCP nodeports 推导每端口 expose_type/external_port。
///
/// expose_type 优先用 annotation（create 时写入，TCP 不对外也能准确区分）；
/// 缺失（旧 app）回退 NodePort 推导：在 NodePort Service 里 = Tcp，否则 Http。
pub(super) fn derive_port_statuses(
    deploy: &Deployment,
    tcp_nodeports: &HashMap<String, u16>,
) -> Vec<AppPortStatus> {
    let port_expose: HashMap<u16, ExposeType> = deploy
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PORT_EXPOSE_ANNOTATION))
        .map(|s| parse_port_expose(s))
        .unwrap_or_default();
    deploy
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.containers.first())
        .and_then(|c| c.ports.as_ref())
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let name = p.name.clone().unwrap_or_default();
                    let port = p.container_port as u16;
                    let (expose_type, external_port) = match port_expose.get(&port) {
                        Some(ExposeType::Tcp) => {
                            (ExposeType::Tcp, tcp_nodeports.get(&name).copied())
                        }
                        Some(ExposeType::Http) => (ExposeType::Http, None),
                        // 回退：无 annotation（旧 app）—— 在 NodePort Service 里 = Tcp，否则 Http
                        None => tcp_nodeports
                            .get(&name)
                            .map_or((ExposeType::Http, None), |np| (ExposeType::Tcp, Some(*np))),
                    };
                    AppPortStatus {
                        name,
                        port,
                        expose_type,
                        external_port,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// 从容器状态提取"启动失败"原因（供 phase=Error 的 message）。
///
/// 命中条件（任一）：
/// - `state.waiting.reason` ∈ {CrashLoopBackOff, ImagePullBackOff, ErrImagePull,
///   CreateContainerConfigError, CreateContainerError, InvalidImageName, RunContainerError,
///   StartError}（`ContainerCreating` 是正常拉起中间态，不在此列，不会被误判）
/// - `state.terminated.exit_code != 0`（容器异常退出）
///
/// CrashLoop 时当前 `state=waiting`，真实退出码在 `last_state.terminated`，一并附带，
/// 便于定位"挂在哪一次退出、退出码多少"。
pub(crate) fn container_error_message(
    cs: &k8s_openapi::api::core::v1::ContainerStatus,
) -> Option<String> {
    let state = cs.state.as_ref()?;
    const BAD_WAITING: &[&str] = &[
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "RunContainerError",
        "StartError",
    ];
    if let Some(w) = state.waiting.as_ref()
        && let Some(reason) = w.reason.as_ref()
        && BAD_WAITING.contains(&reason.as_str())
    {
        let detail = w
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        let term = cs
            .last_state
            .as_ref()
            .and_then(|ls| ls.terminated.as_ref())
            .map(|t| {
                format!(
                    " (last exit={}, reason={})",
                    t.exit_code,
                    t.reason.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_default();
        return Some(format!("{reason}{detail}{term}"));
    }
    if let Some(t) = state.terminated.as_ref()
        && t.exit_code != 0
    {
        let reason = t.reason.as_deref().unwrap_or("");
        let msg = t
            .message
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        return Some(format!(
            "terminated: exit code={exit}, reason={reason}{msg}",
            exit = t.exit_code
        ));
    }
    None
}

/// probes → [`container_runtime_api::AppHealthCheck`] 反推（`build_probe` 的逆映射）。
///
/// readiness http_get → `Http{path, port}`；tcp_socket → `Tcp{port}`；两者皆无 → None。
/// 细节：`build_probe` 对缺省 path 写 "/"，反推 "/" → None；liveness http_get.path 与
/// readiness path 不同时还原 `liveness_path`（相同则 None=复用 path）；delay/period 原样还原。
#[cfg(feature = "kubernetes")]
pub(super) fn probe_to_health_check(
    container: &k8s_openapi::api::core::v1::Container,
) -> Option<container_runtime_api::AppHealthCheck> {
    use container_runtime_api::{AppHealthCheck, HealthCheckType};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    fn probe_port(port: &IntOrString) -> Option<u16> {
        match port {
            IntOrString::Int(i) => u16::try_from(*i).ok(),
            IntOrString::String(s) => s.parse().ok(),
        }
    }

    let readiness = container.readiness_probe.as_ref()?;
    let (check_type, port, path) = if let Some(http) = readiness.http_get.as_ref() {
        let path = http.path.clone().filter(|p| p != "/");
        (HealthCheckType::Http, probe_port(&http.port), path)
    } else {
        let tcp = readiness.tcp_socket.as_ref()?;
        (HealthCheckType::Tcp, probe_port(&tcp.port), None)
    };
    let liveness_path = container
        .liveness_probe
        .as_ref()
        .and_then(|lp| lp.http_get.as_ref())
        .and_then(|lh| lh.path.clone())
        .filter(|lp_path| lp_path != "/" && Some(lp_path) != path.as_ref());
    Some(AppHealthCheck {
        check_type,
        path,
        liveness_path,
        port,
        initial_delay_seconds: readiness
            .initial_delay_seconds
            .and_then(|s| u32::try_from(s).ok()),
        period_seconds: readiness.period_seconds.and_then(|s| u32::try_from(s).ok()),
    })
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
    };

    // ---- derive_phase：四态 + 边界优先级 ----

    #[test]
    fn phase_zero_replicas_is_stopped_regardless_of_error() {
        // replicas=0 最先判断：已缩容，即便容器报 error 也归 Stopped
        assert_eq!(derive_phase(0, 0, &None), "Stopped");
        assert_eq!(derive_phase(0, 0, &Some("CrashLoop".into())), "Stopped");
    }

    #[test]
    fn phase_error_preferred_over_ready() {
        // CrashLoop 期间偶发 ready_replicas>0，error 必须优先 → Error（防误报 Running）
        assert_eq!(derive_phase(1, 1, &Some("err".into())), "Error");
        assert_eq!(derive_phase(2, 2, &Some("err".into())), "Error");
    }

    #[test]
    fn phase_running_when_ready_meets_replicas() {
        assert_eq!(derive_phase(1, 1, &None), "Running");
        assert_eq!(derive_phase(3, 3, &None), "Running");
    }

    #[test]
    fn phase_starting_when_ready_below_replicas() {
        assert_eq!(derive_phase(1, 0, &None), "Starting");
        assert_eq!(derive_phase(3, 1, &None), "Starting");
    }

    #[test]
    fn phase_running_requires_ready_positive() {
        // ready_replicas > 0 是 Running 硬条件：replicas>0 但 ready=0 → Starting
        assert_eq!(derive_phase(2, 0, &None), "Starting");
    }

    // ---- container_error_message：启动失败原因提取 ----

    fn cs_waiting(reason: &str, message: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.to_string()),
                    message: message.map(|m| m.to_string()),
                }),
                ..Default::default()
            }),
            last_state: None,
            ..Default::default()
        }
    }

    fn cs_terminated(exit: i32, reason: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: exit,
                    reason: reason.map(|r| r.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            last_state: None,
            ..Default::default()
        }
    }

    #[test]
    fn error_message_crashloop_with_detail() {
        let cs = cs_waiting("CrashLoopBackOff", Some("back-off 5s"));
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("CrashLoopBackOff: back-off 5s")
        );
    }

    #[test]
    fn error_message_image_pull_no_detail() {
        let cs = cs_waiting("ImagePullBackOff", None);
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("ImagePullBackOff")
        );
    }

    #[test]
    fn error_message_includes_last_terminated_exit_code() {
        // CrashLoop 时当前 state=waiting，真实退出码在 last_state.terminated，应附带
        let mut cs = cs_waiting("CrashLoopBackOff", None);
        cs.last_state = Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 137,
                reason: Some("OOMKilled".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("CrashLoopBackOff (last exit=137, reason=OOMKilled)")
        );
    }

    #[test]
    fn error_message_container_creating_is_none() {
        // ContainerCreating 是正常拉起中间态，不在 BAD_WAITING → None（不误判 Error）
        let cs = cs_waiting("ContainerCreating", None);
        assert!(container_error_message(&cs).is_none());
    }

    #[test]
    fn error_message_nonzero_exit() {
        let cs = cs_terminated(137, Some("OOMKilled"));
        assert_eq!(
            container_error_message(&cs).as_deref(),
            Some("terminated: exit code=137, reason=OOMKilled")
        );
    }

    #[test]
    fn error_message_zero_exit_is_none() {
        let cs = cs_terminated(0, None);
        assert!(container_error_message(&cs).is_none());
    }

    #[test]
    fn error_message_empty_state_is_none() {
        let cs = ContainerStatus {
            state: None,
            ..Default::default()
        };
        assert!(container_error_message(&cs).is_none());
    }

    // ---- derive_port_statuses：annotation 优先 + NodePort 回退 ----

    fn deploy_from_json(json: &str) -> Deployment {
        serde_json::from_str(json).expect("解析测试 Deployment 失败")
    }

    #[test]
    fn port_status_annotation_tcp_with_nodeport() {
        // annotation 标 5432=tcp + NodePort 有该 name → Tcp + external_port
        let deploy = deploy_from_json(
            r#"{"metadata":{"annotations":{"rcoder.io/port-expose":"80:http,5432:tcp"}},
               "spec":{"template":{"spec":{"containers":[{"name":"app","ports":[
                   {"name":"http","containerPort":80},
                   {"name":"pg","containerPort":5432}
               ]}]}}}}"#,
        );
        let mut nodeports = HashMap::new();
        nodeports.insert("pg".to_string(), 30001u16);
        let ports = derive_port_statuses(&deploy, &nodeports);
        assert_eq!(ports.len(), 2);
        let pg = ports.iter().find(|p| p.port == 5432).unwrap();
        assert_eq!(pg.expose_type, ExposeType::Tcp);
        assert_eq!(pg.external_port, Some(30001));
        let http = ports.iter().find(|p| p.port == 80).unwrap();
        assert_eq!(http.expose_type, ExposeType::Http);
        assert_eq!(http.external_port, None);
    }

    #[test]
    fn port_status_no_annotation_falls_back_to_nodeport() {
        // 旧 app 无 annotation：端口在 NodePort Service 里 = Tcp，否则 Http
        let deploy = deploy_from_json(
            r#"{"metadata":{},"spec":{"template":{"spec":{"containers":[{"name":"app","ports":[
                {"name":"http","containerPort":80},
                {"name":"pg","containerPort":5432}
            ]}]}}}}"#,
        );
        let mut nodeports = HashMap::new();
        nodeports.insert("pg".to_string(), 30002u16);
        let ports = derive_port_statuses(&deploy, &nodeports);
        let pg = ports.iter().find(|p| p.port == 5432).unwrap();
        assert_eq!(pg.expose_type, ExposeType::Tcp); // nodeport 里有 → Tcp
        assert_eq!(pg.external_port, Some(30002));
        let http = ports.iter().find(|p| p.port == 80).unwrap();
        assert_eq!(http.expose_type, ExposeType::Http); // nodeport 里无 → Http
    }

    #[test]
    fn port_status_no_container_ports_empty() {
        let deploy = deploy_from_json(
            r#"{"metadata":{"annotations":{"rcoder.io/port-expose":"80:http"}},
               "spec":{"template":{"spec":{"containers":[{"name":"app"}]}}}}"#,
        );
        let ports = derive_port_statuses(&deploy, &HashMap::new());
        assert!(ports.is_empty());
    }
}
