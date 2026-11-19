//! UserApp 镜像漂移观察兜底（spec sandbox-image-version-drift R4，K8s 专有）。
//!
//! 承认 Kubernetes 控制器会先按旧模板创建替换 Pod：这里不做首次注入，只在
//! 「替换 Pod 因旧平台镜像持续无法提供服务」（ImagePullBackOff 跨越判定
//! 窗口）且模板镜像与平台当前引用同仓库（平台管理，归属守卫同 R1）时，
//! 有界触发一次受控镜像对齐。观察到旧镜像自行恢复服务（Running）时不做
//! 任何事；外部/钉住镜像保留；同一 app 冷却期内不重复尝试；对齐后仍失败
//! 只记录，不循环改模板。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::runtime::watcher;
use kube::runtime::watcher::Config as WatcherConfig;
use shared_types::UserAppImageDriftWatchConfig;

use super::k8s_deployment::{
    APP_CONTAINER_NAME, APP_LABEL_PREFIX, APP_MANAGED_BY, RCODER_LABEL_PREFIX,
};

/// 拉取失败类 waiting reason（kubelet 报告的镜像不可得形态）。
/// 只认**业务容器**（`APP_CONTAINER_NAME`，精确名匹配）：任意 sidecar
/// （日志/网格/注入类）拉取失败都不触发对齐——业务容器拉取失败才是模板
/// 漂移的证据（第八轮复审 P1：前缀白名单挡不住普通 sidecar，业务容器
/// Ready 而其他 sidecar 失败时曾对 Deployment 误发 PATCH）。
fn is_image_pull_failure(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .is_some_and(|statuses| {
            statuses.iter().any(|status| {
                if status.name != APP_CONTAINER_NAME {
                    return false;
                }
                status
                    .state
                    .as_ref()
                    .and_then(|state| state.waiting.as_ref())
                    .is_some_and(|waiting| {
                        matches!(
                            waiting.reason.as_deref(),
                            Some("ImagePullBackOff")
                                | Some("ErrImagePull")
                                | Some("ErrImageUploadBackOff")
                                | Some("InvalidImageName")
                        )
                    })
            })
        })
}

fn pod_app_id(pod: &Pod) -> Option<String> {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(&format!("{RCODER_LABEL_PREFIX}/app-id")))
        .filter(|id| !id.is_empty())
        .cloned()
}

/// 启动漂移观察任务。返回句柄由调用方并入后台任务治理。
pub(crate) fn spawn_drift_watch(
    client: kube::Client,
    namespace: String,
    config: UserAppImageDriftWatchConfig,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let platform_repo = container_runtime_api::image_repository(&config.platform_image);
        let window = std::time::Duration::from_secs(config.pull_failure_window_seconds);
        let cooldown = std::time::Duration::from_secs(config.recovery_cooldown_seconds);
        let mut first_failure: HashMap<String, std::time::Instant> = HashMap::new();
        let last_recovery: Arc<Mutex<HashMap<String, std::time::Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        tracing::info!(
            namespace = %namespace,
            platform_image = %config.platform_image,
            window_seconds = config.pull_failure_window_seconds,
            cooldown_seconds = config.recovery_cooldown_seconds,
            "[IMAGE_DRIFT_WATCH] started"
        );
        let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let selector = format!("{APP_LABEL_PREFIX}/managed-by={APP_MANAGED_BY}");
        let watch_config = WatcherConfig::default().labels(&selector).timeout(290);
        let mut stream = Box::pin(watcher(pods, watch_config));
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::info!("[IMAGE_DRIFT_WATCH] stopped");
                    return;
                }
                event = stream.next() => {
                    let Some(event) = event else {
                        // stream 已终结：必须重建 watcher，否则 next() 永远返回
                        // None 退化成告警空转。
                        tracing::warn!("[IMAGE_DRIFT_WATCH] watch stream ended; rebuilding watcher");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
                        let rebuild = WatcherConfig::default().labels(&selector).timeout(290);
                        stream = Box::pin(watcher(pods, rebuild));
                        continue;
                    };
                    let pod = match event {
                        Ok(watcher::Event::Apply(pod)) | Ok(watcher::Event::InitApply(pod)) => pod,
                        Ok(watcher::Event::Delete(pod)) => {
                            // 删除事件：清理该 app 的窗口起点（避免残留 entry）。
                            if let Some(app_id) = pod_app_id(&pod) {
                                first_failure.remove(&app_id);
                            }
                            continue;
                        }
                        Ok(watcher::Event::Init) | Ok(watcher::Event::InitDone) => {
                            // 初始化不携带增量信息；失败窗口由后续 Apply 重建。
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "[IMAGE_DRIFT_WATCH] watch error (watcher retries internally)");
                            continue;
                        }
                    };
                    let Some(app_id) = pod_app_id(&pod) else {
                        continue;
                    };
                    let triggering_pod = pod
                        .metadata
                        .name
                        .clone()
                        .unwrap_or_default();
                    if !is_image_pull_failure(&pod) {
                        first_failure.remove(&app_id);
                        continue;
                    }
                    let started = *first_failure.entry(app_id.clone()).or_insert_with(std::time::Instant::now);
                    if started.elapsed() < window {
                        continue;
                    }
                    first_failure.remove(&app_id);
                    if last_recovery
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&app_id)
                        .is_some_and(|at| at.elapsed() < cooldown)
                    {
                        tracing::warn!(app = %app_id, "[IMAGE_DRIFT_WATCH] recovery on cooldown");
                        continue;
                    }
                    if let Err(error) = recover_drifted_app(
                        client.clone(),
                        &namespace,
                        &app_id,
                        &triggering_pod,
                        &config.platform_image,
                        platform_repo,
                    )
                    .await
                    {
                        tracing::warn!(app = %app_id, %error, "[IMAGE_DRIFT_WATCH] bounded recovery failed");
                    }
                    last_recovery
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(app_id, std::time::Instant::now());
                }
            }
        }
    })
}

/// 单次有界受控对齐：仅平台仓库内的旧模板滚动到当前引用。任何不满足条
/// 件都只记录并返回 Ok（不重试、不循环改模板）。
async fn recover_drifted_app(
    client: kube::Client,
    namespace: &str,
    app_id: &str,
    triggering_pod_name: &str,
    platform_image: &str,
    platform_repo: &str,
) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let name = format!("rcoder-app-{app_id}");
    let deployment = deployments
        .get(&name)
        .await
        .map_err(|error| anyhow::anyhow!("read drifted deployment {name}: {error}"))?;
    // 写前复查（第七轮复审 P1）：Ready 副本已达期望说明业务容器健康——
    // 拉取失败只属于 sidecar 或瞬时抖动，不对齐。
    let ready_replicas = deployment
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let desired_replicas = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    if ready_replicas >= desired_replicas.max(1) {
        tracing::info!(
            app = %app_id,
            ready = ready_replicas,
            desired = desired_replicas,
            "[IMAGE_DRIFT_WATCH] deployment already ready; skipping alignment"
        );
        return Ok(());
    }
    // 写前复查触发 Pod（第八轮复审 P1）：重新读取触发 pod——业务容器必须
    // **此刻仍**拉取失败（窗口期间自愈/被人工修复则不对齐），且其 owner
    // UID 与当前 deployment 一致（换代后旧触发不作数）。
    if !triggering_pod_name.is_empty() {
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        match pods.get(triggering_pod_name).await {
            Ok(current) => {
                // owner 链是 Pod → ReplicaSet → Deployment（实测修正：直接比
                // pod owner 与 Deployment uid 必然不匹配，会把所有对齐挡掉）。
                let owner_matches =
                    match resolve_owner_uid(client.clone(), namespace, &current).await {
                        Some(owner_uid) => {
                            deployment.metadata.uid.as_deref() == Some(owner_uid.as_str())
                        }
                        None => false,
                    };
                if !owner_matches || !is_image_pull_failure(&current) {
                    tracing::info!(
                        app = %app_id,
                        pod = triggering_pod_name,
                        owner_matches,
                        "[IMAGE_DRIFT_WATCH] trigger pod no longer valid; skipping alignment"
                    );
                    return Ok(());
                }
            }
            Err(error) => {
                // 触发 pod 已消失（删除/重建）→ 证据失效，本轮不对齐。
                tracing::info!(
                    app = %app_id,
                    pod = triggering_pod_name,
                    %error,
                    "[IMAGE_DRIFT_WATCH] trigger pod unreadable; skipping alignment"
                );
                return Ok(());
            }
        }
    }
    let managed = deployment
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(&format!("{APP_LABEL_PREFIX}/managed-by")))
        .is_some_and(|value| value == APP_MANAGED_BY);
    let lifecycle_adopted = deployment
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get("rcoder.io/application-id"))
        .is_some();
    if !managed || !lifecycle_adopted {
        anyhow::bail!("deployment {name} is not rcoder-managed lifecycle state");
    }
    let template_image = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .and_then(|spec| {
            spec.containers
                .iter()
                .find(|container| container.name == APP_CONTAINER_NAME)
        })
        .and_then(|container| container.image.clone());
    let Some(template_image) = template_image else {
        anyhow::bail!("deployment {name} has no app container image");
    };
    if template_image == platform_image {
        tracing::info!(
            app = app_id,
            "[IMAGE_DRIFT_WATCH] template already aligned; pull failure is not drift"
        );
        return Ok(());
    }
    if container_runtime_api::image_repository(&template_image) != platform_repo {
        tracing::warn!(
            app = app_id,
            image = %template_image,
            "[IMAGE_DRIFT_WATCH] template image is not platform-managed; kept"
        );
        return Ok(());
    }
    // 前置条件：带上读取到的 resourceVersion——并发用户操作（deploy/stop 的
    // 模板写）与本次对齐互为 409 冲突，由冷却期抑制重试，不做 last-write-wins。
    let resource_version = deployment
        .metadata
        .resource_version
        .clone()
        .unwrap_or_default();
    let patch = serde_json::json!({
        "metadata": {"resourceVersion": resource_version, "annotations": {
            "rcoder.io/image-drift-recovery": format!(
                "auto epoch-{} platform {}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
                platform_image
            )
        }},
        "spec": {"template": {"spec": {"containers": [
            {"name": APP_CONTAINER_NAME, "image": platform_image}
        ]}}}
    });
    deployments
        .patch(
            &name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Strategic(patch),
        )
        .await
        .map_err(|error| anyhow::anyhow!("align drifted deployment {name}: {error}"))?;
    tracing::warn!(
        app = app_id,
        from = %template_image,
        to = %platform_image,
        "[IMAGE_DRIFT_WATCH] platform-managed stale template aligned after persistent pull failure"
    );
    Ok(())
}

/// 沿 Pod → ReplicaSet → Deployment 解析最终 owner UID（None = 链断/非受管形态）。
async fn resolve_owner_uid(client: kube::Client, namespace: &str, pod: &Pod) -> Option<String> {
    use k8s_openapi::api::apps::v1::ReplicaSet;
    let controller = pod
        .metadata
        .owner_references
        .as_ref()?
        .iter()
        .find(|reference| {
            reference.controller.unwrap_or(false) && reference.kind == "ReplicaSet"
        })?;
    let replica_sets: Api<ReplicaSet> = Api::namespaced(client, namespace);
    let replica_set = replica_sets.get(&controller.name).await.ok()?;
    replica_set
        .metadata
        .owner_references
        .as_ref()?
        .iter()
        .find(|reference| reference.controller.unwrap_or(false) && reference.kind == "Deployment")
        .map(|reference| reference.uid.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(json: serde_json::Value) -> Pod {
        serde_json::from_value(json).expect("pod")
    }

    #[test]
    fn pull_failure_detection_covers_kubelet_reasons() {
        let failing = pod(serde_json::json!({
            "metadata": {"name": "p"},
            "status": {"containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": "ImagePullBackOff"}}}
            ]}
        }));
        assert!(is_image_pull_failure(&failing));
        let crash = pod(serde_json::json!({
            "metadata": {"name": "p"},
            "status": {"containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
            ]}
        }));
        assert!(
            !is_image_pull_failure(&crash),
            "业务崩溃不是镜像漂移兜底范围"
        );
        let running = pod(serde_json::json!({
            "metadata": {"name": "p"},
            "status": {"containerStatuses": [
                {"name": "app", "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}}}
            ]}
        }));
        assert!(!is_image_pull_failure(&running));
    }

    #[test]
    fn app_id_reads_managed_label() {
        let labeled = pod(serde_json::json!({
            "metadata": {"labels": {"rcoder.io/app-id": "184"}}
        }));
        assert_eq!(pod_app_id(&labeled).as_deref(), Some("184"));
        assert_eq!(pod_app_id(&pod(serde_json::json!({"metadata": {}}))), None);
    }

    /// 有界恢复三态：平台仓库旧模板滚动；外部镜像与已对齐模板不动。
    #[tokio::test]
    async fn bounded_recovery_rolls_only_platform_managed_stale_templates() {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            for scenario in ["stale", "foreign", "aligned", "unmanaged"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                drop(rustls::crypto::ring::default_provider().install_default());
                let client =
                    kube::Client::try_from(kube::Config::new(format!("http://{address}").parse().unwrap()))
                        .unwrap();
                let image = match scenario {
                    "stale" => "registry.test/app-runtime:0.1.306",
                    "foreign" => "docker.io/someone/custom:1",
                    _ => "registry.test/app-runtime:0.1.456",
                };
                let mut deployment = serde_json::json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {
                        "name": "rcoder-app-9", "uid": "deploy-uid-1", "resourceVersion": "1",
                        "labels": {"app.kubernetes.io/managed-by": "rcoder-app-manager"},
                        "annotations": {"rcoder.io/application-id": "9"}
                    },
                    "spec": {"template": {"spec": {"containers": [
                        {"name": "app", "image": image}
                    ]}}}
                });
                if scenario == "unmanaged" {
                    deployment["metadata"]["labels"]
                        .as_object_mut()
                        .unwrap()
                        .remove("app.kubernetes.io/managed-by");
                }
                let patches = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
                let server_patches = patches.clone();
                let server = tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let (head, offset, length) = loop {
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                let length = head.lines().find_map(|line| {
                                    let (k, v) = line.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().unwrap())
                                }).unwrap_or(0);
                                break (head, end + 4, length);
                            }
                        };
                        while bytes.len() < offset + length {
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        let method = head.split_whitespace().next().unwrap();
                        let path = head.split_whitespace().nth(1).unwrap().split('?').next().unwrap();
                        let (code, value) = if path.ends_with("/deployments/rcoder-app-9") {
                            if method == "PATCH" {
                                let patch: serde_json::Value =
                                    serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
                                server_patches.lock().unwrap().push(patch);
                            }
                            (200, deployment.clone())
                        } else {
                            (404, serde_json::json!({"kind": "Status", "code": 404}))
                        };
                        let body = value.to_string();
                        let response = format!(
                            "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
                let result = recover_drifted_app(
                    client,
                    "drift-test",
                    "9",
                    "",  // 无触发 pod 名 → 跳过 pod 复核（单元测试聚焦 patch 语义）
                    "registry.test/app-runtime:0.1.456",
                    "registry.test/app-runtime",
                )
                .await;
                server.abort();
                let writes = patches.lock().unwrap().clone();
                match scenario {
                    "stale" => {
                        result.expect("stale rolls");
                        assert_eq!(writes.len(), 1);
                        assert_eq!(
                            writes[0]["spec"]["template"]["spec"]["containers"][0]["image"],
                            "registry.test/app-runtime:0.1.456"
                        );
                        assert_eq!(
                            writes[0]["metadata"]["resourceVersion"], "1",
                            "对齐写必须携带读取时的 resourceVersion 前置条件"
                        );
                    }
                    "foreign" | "aligned" => {
                        result.expect("keep is Ok without writes");
                        assert!(writes.is_empty(), "{scenario} must not patch");
                    }
                    "unmanaged" => {
                        assert!(result.is_err(), "unmanaged workload rejected");
                        assert!(writes.is_empty());
                    }
                    _ => unreachable!(),
                }
            }
        })
        .await
        .unwrap();
    }

    /// 触发 pod 复核的 owner 链反例（221aa662 回归）：真实 K8s 中 Pod 的
    /// controller owner 是 ReplicaSet 而非 Deployment——修复前直接比对 pod
    /// owner uid 与 deployment uid，两级链必然不匹配，全部对齐被跳过
    /// （旧逻辑下 chain-matched 场景 0 次 PATCH，本测试随之失败）。
    #[tokio::test]
    async fn two_level_owner_chain_gates_trigger_pod_recheck() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            for (scenario, pod_waiting, rs_owner_uid, expect_patch) in [
                // 两级链 Pod→RS→Deployment 归属一致且业务容器仍拉取失败 → 必须对齐
                ("chain-matched", true, "deploy-uid-1", true),
                // RS 的 Deployment owner 已换代（uid 不同）→ 旧触发不作数
                ("chain-superseded", true, "old-deploy-uid", false),
                // 窗口期内 pod 已自愈 Running → 证据失效，不对齐
                ("pod-recovered", false, "deploy-uid-1", false),
            ] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                drop(rustls::crypto::ring::default_provider().install_default());
                let client =
                    kube::Client::try_from(kube::Config::new(format!("http://{address}").parse().unwrap()))
                        .unwrap();
                let deployment = serde_json::json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {
                        "name": "rcoder-app-9", "uid": "deploy-uid-1", "resourceVersion": "1",
                        "labels": {"app.kubernetes.io/managed-by": "rcoder-app-manager"},
                        "annotations": {"rcoder.io/application-id": "9"}
                    },
                    "spec": {"template": {"spec": {"containers": [
                        {"name": "app", "image": "registry.test/app-runtime:0.1.306"}
                    ]}}}
                });
                let container_state = if pod_waiting {
                    serde_json::json!({"waiting": {"reason": "ImagePullBackOff"}})
                } else {
                    serde_json::json!({"running": {"startedAt": "2026-01-01T00:00:00Z"}})
                };
                let trigger_pod = serde_json::json!({
                    "apiVersion": "v1", "kind": "Pod",
                    "metadata": {
                        "name": "trigger-pod",
                        "ownerReferences": [{
                            "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "rs-1",
                            "uid": "rs-uid-1", "controller": true
                        }]
                    },
                    "status": {"containerStatuses": [
                        {"name": "app", "state": container_state}
                    ]}
                });
                let replica_set = serde_json::json!({
                    "apiVersion": "apps/v1", "kind": "ReplicaSet",
                    "metadata": {
                        "name": "rs-1", "uid": "rs-uid-1",
                        "ownerReferences": [{
                            "apiVersion": "apps/v1", "kind": "Deployment", "name": "rcoder-app-9",
                            "uid": rs_owner_uid, "controller": true
                        }]
                    }
                });
                let patches = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
                let server_patches = patches.clone();
                let server = tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let (head, offset, length) = loop {
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                let length = head.lines().find_map(|line| {
                                    let (k, v) = line.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().unwrap())
                                }).unwrap_or(0);
                                break (head, end + 4, length);
                            }
                        };
                        while bytes.len() < offset + length {
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        let method = head.split_whitespace().next().unwrap();
                        let path = head.split_whitespace().nth(1).unwrap().split('?').next().unwrap();
                        let (code, value) = if path.ends_with("/deployments/rcoder-app-9") {
                            if method == "PATCH" {
                                let patch: serde_json::Value =
                                    serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
                                server_patches.lock().unwrap().push(patch);
                            }
                            (200, deployment.clone())
                        } else if path.ends_with("/pods/trigger-pod") {
                            (200, trigger_pod.clone())
                        } else if path.ends_with("/replicasets/rs-1") {
                            (200, replica_set.clone())
                        } else {
                            (404, serde_json::json!({"kind": "Status", "code": 404}))
                        };
                        let body = value.to_string();
                        let response = format!(
                            "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
                let result = recover_drifted_app(
                    client,
                    "drift-test",
                    "9",
                    "trigger-pod",
                    "registry.test/app-runtime:0.1.456",
                    "registry.test/app-runtime",
                )
                .await;
                server.abort();
                result.expect("recheck outcomes are Ok (skip or align)");
                let writes = patches.lock().unwrap().clone();
                if expect_patch {
                    assert_eq!(
                        writes.len(),
                        1,
                        "{scenario}: Pod→ReplicaSet→Deployment 两级链不得挡住对齐"
                    );
                    assert_eq!(
                        writes[0]["spec"]["template"]["spec"]["containers"][0]["image"],
                        "registry.test/app-runtime:0.1.456"
                    );
                } else {
                    assert!(writes.is_empty(), "{scenario} must not patch");
                }
            }
        })
        .await
        .unwrap();
    }

    /// 非受管形态直接判 None（不发起任何 K8s 请求）：无 ownerReferences，
    /// 或 controller owner 不是 ReplicaSet（如 StatefulSet 直管 Pod）。
    #[tokio::test]
    async fn resolve_owner_uid_requires_replicaset_controller() {
        drop(rustls::crypto::ring::default_provider().install_default());
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap()))
                .unwrap();
        let bare = pod(serde_json::json!({"metadata": {"name": "p"}}));
        assert_eq!(resolve_owner_uid(client.clone(), "ns", &bare).await, None);
        let stateful = pod(serde_json::json!({
            "metadata": {"name": "p", "ownerReferences": [{
                "apiVersion": "apps/v1", "kind": "StatefulSet", "name": "sts",
                "uid": "sts-uid", "controller": true
            }]}
        }));
        assert_eq!(resolve_owner_uid(client, "ns", &stateful).await, None);
    }
}
