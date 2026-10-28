//! 存量无标签 Web agent 容器名字反解纳管的 wire 契约（fake Docker HTTP）。
//!
//! 锁住的偏移敏感点：
//! - 发现守卫：前缀精确回构、最长前缀族解析（防吞 Userapp/Builder 名）、
//!   带标签 CID 去重、缺管理双标签、Running
//! - 退休 legacy 分支：无管理标签 → 精确 CID + 观测名 + 名字反解身份；
//!   改名/换标识拒绝；带标签容器仍走标签严格等值（原语义）
use super::*;
use container_runtime_api::AgentContainerRuntime;
use std::collections::HashMap as StdHashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type InspectMap = Arc<Mutex<StdHashMap<String, serde_json::Value>>>;

struct FakeDocker {
    label_list: serde_json::Value,
    name_list: serde_json::Value,
    inspects: InspectMap,
    requests: Arc<Mutex<Vec<String>>>,
}

async fn legacy_runtime(
    fake: FakeDocker,
    web_prefix: Option<&str>,
) -> (
    DockerRuntime,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = fake.requests.clone();
    let recorded = fake.requests.clone();
    let inspects = fake.inspects.clone();
    let label_list = fake.label_list.clone();
    let name_list = fake.name_list.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                match stream.read_u8().await {
                    Ok(byte) => bytes.push(byte),
                    Err(_) => break,
                }
            }
            let first = String::from_utf8_lossy(&bytes)
                .lines()
                .next()
                .unwrap_or("")
                .to_owned();
            recorded.lock().unwrap().push(first.clone());
            let (code, response) = if first.contains("/containers/json") {
                // 两次 list 用 filters 的 JSON 键区分（URL 编码后 %22label%22 / %22name%22）
                if first.contains("%22label%22") {
                    (200, label_list.clone())
                } else {
                    (200, name_list.clone())
                }
            } else if first.starts_with("DELETE ") {
                // 删除后确认 inspect 应 404：从应答表移除该 CID
                if let Some(cid) = first
                    .strip_prefix("DELETE /containers/")
                    .and_then(|rest| rest.split(['?', '/']).next())
                {
                    inspects.lock().unwrap().remove(cid);
                }
                (204, serde_json::Value::Null)
            } else if let Some(cid) = first
                .strip_prefix("GET /containers/")
                .and_then(|rest| rest.split('/').next())
                .map(str::to_string)
            {
                match inspects.lock().unwrap().get(&cid) {
                    Some(value) => (200, value.clone()),
                    None => (404, serde_json::json!({"message": "no such container"})),
                }
            } else {
                (404, serde_json::json!({"message": "unhandled"}))
            };
            let body = if code == 204 {
                String::new()
            } else {
                response.to_string()
            };
            let _written = stream
                .write_all(
                    format!(
                        "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let mut config = crate::types::DockerManagerConfig::default();
    if let Some(prefix) = web_prefix {
        let service: shared_types::ServiceImageConfig = serde_json::from_value(serde_json::json!({
            "service_type": "web-agent-runner",
            "image_tag_prefix": prefix,
            "enabled": true
        }))
        .unwrap();
        config
            .multi_image_config
            .services
            .insert("web-agent-runner".into(), service);
    }
    let manager = Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config,
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    (DockerRuntime::new(manager), requests, server)
}

fn running_inspect(cid: &str, name: &str, started_at: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": cid,
        "Name": format!("/{name}"),
        "State": {"Running": true, "StartedAt": started_at, "Status": "running"},
        "Config": {},
        "NetworkSettings": {"Networks": {"test": {"IPAddress": "172.18.0.9"}}},
        "HostConfig": {}
    })
}

fn stopped_inspect(cid: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": cid,
        "Name": format!("/{name}"),
        "State": {"Running": false, "StartedAt": "2026-10-01T00:00:00Z", "Status": "exited"},
        "Config": {},
        "NetworkSettings": {"Networks": {}},
        "HostConfig": {}
    })
}

fn inspects_of(entries: Vec<(String, serde_json::Value)>) -> InspectMap {
    Arc::new(Mutex::new(entries.into_iter().collect()))
}

fn requests_recorder() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn delete_count(requests: &Arc<Mutex<Vec<String>>>, prefix: &str) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.starts_with(prefix))
        .count()
}

/// 无标签、Running、前缀回构命中的容器被纳管为 Web 快照。
#[tokio::test]
async fn legacy_unlabeled_running_web_agent_is_adopted() {
    let requests = requests_recorder();
    let (runtime, _, server) = legacy_runtime(
        FakeDocker {
            label_list: serde_json::json!([]),
            name_list: serde_json::json!([
                {"Id":"legacy-cid-1","Names":["/rcoder-1841779593646080"],"Created":1}
            ]),
            inspects: inspects_of(vec![(
                "legacy-cid-1".to_string(),
                running_inspect(
                    "legacy-cid-1",
                    "rcoder-1841779593646080",
                    "2026-10-01T00:00:00Z",
                ),
            )]),
            requests,
        },
        Some("rcoder"),
    )
    .await;

    let inventory = runtime.docker_agent_inventory().await.unwrap();

    assert_eq!(inventory.len(), 1, "无标签 Running 候选必须被纳管");
    let snapshot = &inventory[0];
    assert_eq!(snapshot.identifier, "1841779593646080");
    assert_eq!(snapshot.service_type, ServiceType::WebAgentRunner);
    let container = snapshot.container.as_ref().unwrap();
    assert_eq!(container.uid, "legacy-cid-1");
    assert_eq!(container.name, "rcoder-1841779593646080");
    assert_eq!(
        container.resource_version.as_deref(),
        Some("2026-10-01T00:00:00Z")
    );
    assert_eq!(
        snapshot.endpoint.as_deref(),
        Some("http://172.18.0.9:50051"),
        "纳管必须带探活端点"
    );
    server.abort();
}

/// 守卫矩阵：非 Running / 带双标签 / Userapp 名 / 回构失败 / 与带标签集
/// 同 CID —— 全部不纳管。
#[tokio::test]
async fn legacy_adoption_guards_reject_non_conforming_candidates() {
    let requests = requests_recorder();
    let mut labeled = running_inspect("legacy-labeled", "rcoder-333", "2026-10-01T00:00:00Z");
    labeled["Config"]["Labels"] = serde_json::json!({
        "identifier": "333", "service-type": "web-agent-runner"
    });
    let (runtime, _, server) = legacy_runtime(
        FakeDocker {
            // labeled 集里已有 legacy-labeled（同 CID 去重 + 双标签域守卫用）
            label_list: serde_json::json!([
                {"Id":"legacy-labeled","Names":["/rcoder-333"],"Created":1,
                 "Labels":{"identifier":"333","service-type":"web-agent-runner"}}
            ]),
            name_list: serde_json::json!([
                {"Id":"legacy-stopped","Names":["/rcoder-111"],"Created":1},
                {"Id":"legacy-userapp","Names":["/rcoder-app-39"],"Created":1},
                {"Id":"legacy-badname","Names":["/rcoder-not a valid id!"],"Created":1},
                {"Id":"legacy-labeled","Names":["/rcoder-333"],"Created":1,
                 "Labels":{"identifier":"333","service-type":"web-agent-runner"}},
                {"Id":"legacy-nolabels-ok","Names":["/rcoder-222"],"Created":1}
            ]),
            inspects: inspects_of(vec![
                (
                    "legacy-stopped".into(),
                    stopped_inspect("legacy-stopped", "rcoder-111"),
                ),
                (
                    "legacy-userapp".into(),
                    running_inspect("legacy-userapp", "rcoder-app-39", "2026-10-01T00:00:00Z"),
                ),
                (
                    "legacy-badname".into(),
                    running_inspect(
                        "legacy-badname",
                        "rcoder-not a valid id!",
                        "2026-10-01T00:00:00Z",
                    ),
                ),
                ("legacy-labeled".into(), labeled),
                (
                    "legacy-nolabels-ok".into(),
                    running_inspect("legacy-nolabels-ok", "rcoder-222", "2026-10-01T00:00:00Z"),
                ),
            ]),
            requests,
        },
        Some("rcoder"),
    )
    .await;

    let inventory = runtime.docker_agent_inventory().await.unwrap();

    // 标签路径的 legacy-labeled（标签域）+ legacy-nolabels-ok（纳管），
    // 其余全部被守卫拒绝
    assert_eq!(
        inventory.len(),
        2,
        "非 Running/Userapp 名/回构失败/同 CID 去重后仅纳管合法无标签候选"
    );
    let identifiers: Vec<&str> = inventory.iter().map(|s| s.identifier.as_str()).collect();
    assert!(identifiers.contains(&"333"));
    assert!(identifiers.contains(&"222"));
    assert!(!identifiers.contains(&"111"), "非 Running 不纳管");
    assert!(!identifiers.contains(&"39"), "Userapp 名不被吞为 Web");
    server.abort();
}

/// 退休 legacy 分支：无标签快照按精确 CID + 观测名 + 名字反解复核；
/// 改名后拒绝删除。
#[tokio::test]
async fn legacy_retirement_verifies_name_and_cid_and_rejects_rename() {
    let requests = requests_recorder();
    let inspects = inspects_of(vec![(
        "legacy-cid-2".to_string(),
        running_inspect("legacy-cid-2", "rcoder-42", "2026-10-01T00:00:00Z"),
    )]);
    let (runtime, recorded, server) = legacy_runtime(
        FakeDocker {
            label_list: serde_json::json!([]),
            name_list: serde_json::json!([
                {"Id":"legacy-cid-2","Names":["/rcoder-42"],"Created":1}
            ]),
            inspects: inspects.clone(),
            requests: requests.clone(),
        },
        Some("rcoder"),
    )
    .await;
    let snapshot = runtime.docker_agent_inventory().await.unwrap().remove(0);
    // stop_captured_agent = 非自动退休（无 automatic 证据要求），验证归属复核
    let outcome = AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot)
        .await
        .unwrap();
    assert_eq!(outcome, AgentStopOutcome::Complete);
    assert_eq!(
        delete_count(&recorded, "DELETE /containers/legacy-cid-2"),
        1
    );
    server.abort();
    drop(server.await);

    // 改名场景：捕获后容器被 docker rename → 退休时观测名与快照不一致 → 拒绝
    let requests = requests_recorder();
    let inspects = inspects_of(vec![(
        "legacy-cid-3".to_string(),
        running_inspect("legacy-cid-3", "rcoder-43", "2026-10-01T00:00:00Z"),
    )]);
    let (runtime, recorded, server) = legacy_runtime(
        FakeDocker {
            label_list: serde_json::json!([]),
            name_list: serde_json::json!([
                {"Id":"legacy-cid-3","Names":["/rcoder-43"],"Created":1}
            ]),
            inspects: inspects.clone(),
            requests: requests.clone(),
        },
        Some("rcoder"),
    )
    .await;
    let mut snapshot = runtime.docker_agent_inventory().await.unwrap().remove(0);
    // 捕获后改名（inspect 应答同步换名，模拟 docker rename）
    inspects.lock().unwrap().insert(
        "legacy-cid-3".to_string(),
        running_inspect("legacy-cid-3", "renamed-elsewhere", "2026-10-01T00:00:00Z"),
    );
    snapshot.container.as_mut().unwrap().name = "rcoder-43".into();
    let result = AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot).await;
    assert!(result.is_err(), "改名后必须拒绝：{result:?}");
    assert_eq!(delete_count(&recorded, "DELETE "), 0, "拒绝时零删除");
    server.abort();
}

/// 带标签容器的退休仍走标签严格等值（Foreign 拒绝，原语义不回归）。
#[tokio::test]
async fn labeled_retirement_keeps_label_verification() {
    let requests = requests_recorder();
    let mut owner = running_inspect("labeled-cid", "rcoder-77", "2026-10-01T00:00:00Z");
    owner["Config"]["Labels"] =
        serde_json::json!({"identifier": "owner", "service-type": "web-agent-runner"});
    let inspects = inspects_of(vec![("labeled-cid".to_string(), owner)]);
    let (runtime, recorded, server) = legacy_runtime(
        FakeDocker {
            label_list: serde_json::json!([
                {"Id":"labeled-cid","Names":["/rcoder-77"],"Created":1,
                 "Labels":{"identifier":"owner","service-type":"web-agent-runner"}}
            ]),
            name_list: serde_json::json!([]),
            inspects: inspects.clone(),
            requests,
        },
        Some("rcoder"),
    )
    .await;
    let snapshot = runtime.docker_agent_inventory().await.unwrap().remove(0);
    assert_eq!(snapshot.identifier, "owner");
    // 捕获后同名容器换代（CID 不变不可行，模拟标签面变化）：退休时标签
    // 与快照不一致 → Conflict，零删除
    let mut foreign = running_inspect("labeled-cid", "rcoder-77", "2026-10-01T00:00:00Z");
    foreign["Config"]["Labels"] =
        serde_json::json!({"identifier": "foreign", "service-type": "web-agent-runner"});
    inspects
        .lock()
        .unwrap()
        .insert("labeled-cid".to_string(), foreign);
    let result = AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot).await;
    assert!(result.is_err(), "标签不匹配必须拒绝");
    assert_eq!(delete_count(&recorded, "DELETE "), 0, "拒绝时零删除");
    server.abort();
}

/// 部分身份标签也属于归属证据，不能被名字兜底覆盖。
#[tokio::test]
async fn legacy_conflicting_partial_labels_are_never_adopted_or_deleted() {
    for labels in [
        serde_json::json!({"identifier": "foreign"}),
        serde_json::json!({"service-type": "computer-agent-runner"}),
        serde_json::json!({"service-type": "invalid"}),
        serde_json::json!({"managed-by": "foreign-controller"}),
    ] {
        let mut inspected = running_inspect("partial-cid", "rcoder-42", "2026-10-01T00:00:00Z");
        inspected["Config"]["Labels"] = labels.clone();
        let (runtime, recorded, server) = legacy_runtime(
            FakeDocker {
                label_list: serde_json::json!([]),
                name_list: serde_json::json!([{
                    "Id": "partial-cid", "Names": ["/rcoder-42"], "Created": 1,
                    "Labels": labels
                }]),
                inspects: inspects_of(vec![("partial-cid".into(), inspected)]),
                requests: requests_recorder(),
            },
            Some("rcoder"),
        )
        .await;
        let inventory = runtime.docker_agent_inventory().await.unwrap();
        assert!(
            inventory.is_empty(),
            "conflicting partial labels must not be adopted: {labels}"
        );
        // 直接传入历史快照也必须复核，不能仅依赖发现层。
        let snapshot = AgentStopSnapshot {
            app_id_hint: None,
            namespace: Some(runtime.agent_retirement_namespace()),
            identifier: "42".into(),
            service_type: ServiceType::WebAgentRunner,
            controller: None,
            pods: vec![],
            container: Some(AgentResourceIdentity {
                kind: AgentResourceKind::Container,
                name: "rcoder-42".into(),
                uid: "partial-cid".into(),
                resource_version: Some("2026-10-01T00:00:00Z".into()),
                created_at: None,
                owner_uid: None,
            }),
            services: vec![],
            replicas: None,
            endpoint: None,
            compute_data_protection: None,
        };
        assert!(
            AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot)
                .await
                .is_err()
        );
        assert_eq!(delete_count(&recorded, "DELETE "), 0);
        server.abort();
    }
}

#[tokio::test]
async fn legacy_ambiguous_web_and_userapp_prefix_is_not_adopted() {
    let (runtime, _, server) = legacy_runtime(FakeDocker {
        label_list: serde_json::json!([]),
        name_list: serde_json::json!([{"Id":"ambiguous-cid", "Names":["/rcoder-app-42"], "Created":1}]),
        inspects: inspects_of(vec![("ambiguous-cid".into(), running_inspect("ambiguous-cid", "rcoder-app-42", "2026-10-01T00:00:00Z"))]),
        requests: requests_recorder(),
    }, Some("rcoder-app")).await;
    assert!(
        runtime.docker_agent_inventory().await.unwrap().is_empty(),
        "a name cannot resolve conflicting service families"
    );
    server.abort();
}

#[tokio::test]
async fn legacy_inspect_labels_are_authoritative_and_matching_partial_labels_work() {
    for (listed, observed, accepted) in [
        (
            serde_json::json!({}),
            serde_json::json!({"identifier":"foreign"}),
            false,
        ),
        (
            serde_json::json!({"identifier":"42"}),
            serde_json::json!({"identifier":"42"}),
            true,
        ),
        (
            serde_json::json!({"service-type":"web-agent-runner"}),
            serde_json::json!({"service-type":"web-agent-runner"}),
            true,
        ),
    ] {
        let mut inspected = running_inspect("partial-cid", "rcoder-42", "2026-10-01T00:00:00Z");
        inspected["Config"]["Labels"] = observed;
        let (runtime, recorded, server) = legacy_runtime(FakeDocker {
            label_list: serde_json::json!([]),
            name_list: serde_json::json!([{"Id":"partial-cid", "Names":["/rcoder-42"], "Created":1, "Labels":listed}]),
            inspects: inspects_of(vec![("partial-cid".into(), inspected)]),
            requests: requests_recorder(),
        }, Some("rcoder")).await;
        let mut inventory = runtime.docker_agent_inventory().await.unwrap();
        assert_eq!(inventory.len(), usize::from(accepted));
        if accepted {
            assert_eq!(
                AgentContainerRuntime::stop_captured_agent(&runtime, &inventory.remove(0))
                    .await
                    .unwrap(),
                AgentStopOutcome::Complete
            );
            assert_eq!(delete_count(&recorded, "DELETE /containers/partial-cid"), 1);
        } else {
            assert_eq!(delete_count(&recorded, "DELETE "), 0);
        }
        server.abort();
    }
}
