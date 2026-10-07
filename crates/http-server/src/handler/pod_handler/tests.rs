use super::helpers::*;
use super::types::*;
use super::*;

#[test]
fn test_pod_count_by_service_type_default() {
    let count = PodCountByServiceType {
        rcoder: 0,
        computer_agent_runner: 0,
    };
    assert_eq!(count.rcoder + count.computer_agent_runner, 0);
}

#[test]
fn test_pod_resource_limits_serialization() {
    let limits = ServiceResourceLimits {
        memory: Some(4294967296.0),
        cpu: Some(2.0),
        swap: Some(6442450944.0),
        storage_size: Some("10Gi".to_string()),
        ephemeral_storage_limit: None,
    };

    let json = serde_json::to_string(&limits).unwrap();
    assert!(json.contains("4294967296"));
    assert!(json.contains("2.0"));
    assert!(json.contains("6442450944"));
    assert!(json.contains("10Gi"));
}

#[test]
fn test_ensure_pod_response_serialization() {
    let response = EnsurePodResponse {
        created: true,
        container_info: PodContainerInfo {
            container_id: "abc123".to_string(),
            status: "running".to_string(),
        },
        message: "容器创建成功".to_string(),
    };

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("created"));
    assert!(json.contains("container_info"));
    assert!(json.contains("message"));
}

#[test]
fn test_validate_resource_limits_valid() {
    let limits = ServiceResourceLimits {
        memory: Some(4294967296.0), // 4GB
        cpu: Some(2.0),
        swap: Some(6442450944.0), // 6GB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_none_values() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_cpu_zero() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_negative() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(-1.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_too_large() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(200.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_memory_too_small() {
    let limits = ServiceResourceLimits {
        memory: Some(256_000_000.0), // 256MB
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_memory_too_large() {
    let limits = ServiceResourceLimits {
        memory: Some(256_000_000_000.0), // 256GB
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_swap_less_than_memory() {
    let limits = ServiceResourceLimits {
        memory: Some(8_589_934_592.0), // 8GB
        cpu: None,
        swap: Some(4_294_967_296.0), // 4GB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    // swap<memory 已改为 resolve 阶段 normalize_swap 自动规整,validate 不再拒绝
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_swap_too_small() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: None,
        swap: Some(256_000_000.0), // 256MB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_boundary() {
    // 测试边界值：0.1 应该失败（小于等于 0）
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.1),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());

    // 测试边界值：0.01 应该通过
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.01),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

// ============================================================================
// userApp 分派（parse_app_target）
// ============================================================================

#[test]
fn app_target_no_app_id_falls_through_to_agent_path() {
    // 无 app_id 无 app_stage → agent/computer 既有路径
    assert!(matches!(
        parse_app_target(None, None, Some("computer-agent-runner")),
        Ok(AppTarget::NotApp)
    ));
}

#[test]
fn app_target_dev_and_prod_dispatch() {
    assert!(matches!(
        parse_app_target(Some("app1"), None, None),
        Ok(AppTarget::Dev(id)) if id == "app1"
    ));
    assert!(matches!(
        parse_app_target(Some("app1"), Some("dev"), None),
        Ok(AppTarget::Dev(id)) if id == "app1"
    ));
    assert!(matches!(
        parse_app_target(Some("app1"), Some("prod"), None),
        Ok(AppTarget::Prod(id)) if id == "app1"
    ));
    // 空串 app_id 视为未传（回 agent 路径）
    assert!(matches!(
        parse_app_target(Some("  "), None, None),
        Ok(AppTarget::NotApp)
    ));
}

#[test]
fn app_target_validates_stage_and_conflicts() {
    // app_id 与非 userapp 的 service_type 互斥
    assert!(parse_app_target(Some("app1"), None, Some("computer-agent-runner")).is_err());
    // 非法 stage 值
    assert!(parse_app_target(Some("app1"), Some("staging"), None).is_err());
    // app_stage 依附于 app_id
    assert!(parse_app_target(None, Some("dev"), None).is_err());
    // identifier 白名单（防容器名/bind 路径注入）
    assert!(parse_app_target(Some("../escape"), None, None).is_err());
    assert!(parse_app_target(Some("app/1"), None, None).is_err());
}

/// userApp 场景统一三字段形态：service_type=userapp（大小写不敏感）与 app_id
/// 搭配放行分派；无 app_id 时单独的 userapp 标记报错（防误走 agent 路径空查）。
#[test]
fn app_target_accepts_userapp_service_type_alongside_app_id() {
    // userapp 搭配 app_id → 正常分派（缺省/显式 dev 与 prod）
    assert!(matches!(
        parse_app_target(Some("app1"), None, Some("userapp")),
        Ok(AppTarget::Dev(id)) if id == "app1"
    ));
    assert!(matches!(
        parse_app_target(Some("app1"), Some("prod"), Some("userapp")),
        Ok(AppTarget::Prod(id)) if id == "app1"
    ));
    // 大小写不敏感 + 既有 ServiceType 变体同义
    for variant in ["USERAPP", "Userapp", "user-app"] {
        assert!(
            matches!(
                parse_app_target(Some("app1"), None, Some(variant)),
                Ok(AppTarget::Dev(_))
            ),
            "service_type={variant:?} 应视为 userapp 变体放行"
        );
    }
    // userapp 标记缺 app_id → 报错（不走 agent 路径空查 Userapp 容器）
    assert!(parse_app_target(None, None, Some("userapp")).is_err());
}

/// 契约钉住：userApp 请求只传 app_id/app_stage 即可反序列化（user_id/project_id
/// 有 serde default 兜底，agent 路径空值校验在后）——Java 最小请求形态。
#[test]
fn userapp_minimal_request_deserializes_without_user_or_project() {
    for raw in [
        r#"{"app_id":"app1"}"#,
        r#"{"app_id":"app1","app_stage":"dev"}"#,
        r#"{"app_id":"app1","app_stage":"prod"}"#,
    ] {
        let ensured: EnsurePodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("EnsurePodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(ensured.user_id, "");
        assert_eq!(ensured.app_id.as_deref(), Some("app1"));
        let ka: KeepalivePodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("KeepalivePodRequest {raw} 应可反序列化: {e}"));
        assert!(ka.app_stage.is_some() || ka.app_stage.is_none());
        let rs: RestartPodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("RestartPodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(rs.project_id, "");
        let sp: StopPodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("StopPodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(sp.user_id, "");
        assert_eq!(sp.app_id.as_deref(), Some("app1"));
    }
}

/// 契约钉住：stop 的 agent 路径完整形态（user_id/project_id/service_type）+
/// query 形态（I18nJsonOrQuery 兜底）均可反序列化。
#[test]
fn stop_pod_request_deserializes_agent_path_forms() {
    let sp: StopPodRequest = serde_json::from_str(
        r#"{"user_id":"user_123","project_id":"proj_456","service_type":"web-agent-runner"}"#,
    )
    .unwrap_or_else(|e| panic!("StopPodRequest agent 形态应可反序列化: {e}"));
    assert_eq!(sp.user_id, "user_123");
    assert_eq!(sp.project_id, "proj_456");
    assert_eq!(sp.service_type.as_deref(), Some("web-agent-runner"));
    assert!(sp.pod_id.is_none() && sp.app_id.is_none() && sp.app_stage.is_none());

    let qs: StopPodRequest = serde_urlencoded::from_str("user_id=user_123&project_id=proj_456")
        .unwrap_or_else(|e| panic!("StopPodRequest query 形态应可反序列化: {e}"));
    assert_eq!(qs.user_id, "user_123");
    assert_eq!(qs.project_id, "proj_456");
}

/// 契约钉住：GET 两接口的 userApp 三字段 query 形态可反序列化（userapp 与
/// app_id/app_stage 搭配；user_id/project_id 不传为 None）——Java 统一传参形态。
#[test]
fn userapp_query_deserializes_with_three_field_form() {
    // serde_urlencoded 是 axum Query 底层（I18nQuery 纯透传），用同一引擎验证。
    for raw in [
        "service_type=userapp&app_id=app-1",
        "service_type=userapp&app_id=app-1&app_stage=dev",
        "service_type=userapp&app_id=app-1&app_stage=prod",
    ] {
        let ps: PodStatusQuery = serde_urlencoded::from_str(raw)
            .unwrap_or_else(|e| panic!("PodStatusQuery {raw} 应可反序列化: {e}"));
        assert_eq!(ps.service_type.as_deref(), Some("userapp"));
        assert_eq!(ps.app_id.as_deref(), Some("app-1"));
        assert!(ps.user_id.is_none() && ps.project_id.is_none());

        let vs: VncStatusQuery = serde_urlencoded::from_str(raw)
            .unwrap_or_else(|e| panic!("VncStatusQuery {raw} 应可反序列化: {e}"));
        assert_eq!(vs.service_type.as_deref(), Some("userapp"));
        assert_eq!(vs.app_id.as_deref(), Some("app-1"));
    }
    // 仅 app_id（service_type/app_stage 缺省）也可——与 POST 三兄弟最小契约对齐
    let ps: PodStatusQuery = serde_urlencoded::from_str("app_id=app-1")
        .unwrap_or_else(|e| panic!("PodStatusQuery 应可反序列化: {e}"));
    assert_eq!(ps.app_id.as_deref(), Some("app-1"));
    assert!(ps.service_type.is_none() && ps.app_stage.is_none());
}

#[cfg(feature = "userapp-turso")]
mod compute_error_route_tests {
    use super::super::{pod_restart, pod_stop};
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::post,
    };
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError,
        ContainerRuntimeResult, DeploymentStatus, RuntimeContainerInfo, UserAppDeploymentRuntime,
        WorkspaceRuntime,
    };
    use rcoder_storage::userapp_lifecycle::TursoUserAppStore;
    use shared_types::{
        AppResourceIdentity, AppResourceKind, BuilderControlTarget, ComputeControlAction,
        ComputeControlRecord, ComputeControlRequest, ContainerBasicInfo, ServiceType,
        UserAppComputeStatus, UserAppExecutionContext, UserAppLifecycleRecord,
        UserAppLifecycleStore as _, UserAppOperationScope,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt as _;

    // Controlled runtime protocol fixture. The store, submit coordinator,
    // extractors, actual handlers, and HTTP envelope conversion are production.
    // No HTTP response or AI result is synthesized by the fixture.
    #[derive(Default)]
    struct ReadOnlyRuntime {
        writes: AtomicUsize,
    }
    #[async_trait]
    impl AgentContainerRuntime for ReadOnlyRuntime {
        async fn create_container(
            &self,
            _: ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Err(ContainerRuntimeError::ConfigurationError(
                "unexpected create".into(),
            ))
        }
        async fn get_container_info(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
        async fn find_container(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            Ok(None)
        }
        async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Err(ContainerRuntimeError::ConfigurationError(
                "unexpected stop".into(),
            ))
        }
        async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn inspect_builder_candidate(
            &self,
            context: &UserAppExecutionContext,
        ) -> ContainerRuntimeResult<BuilderControlTarget> {
            Ok(BuilderControlTarget {
                resource_binding: None,
                context: context.clone(),
                workload: Some(AppResourceIdentity {
                    kind: AppResourceKind::StatefulSet,
                    name: "rcoder-app-builder-routeapp".into(),
                    uid: "retained-sts".into(),
                    resource_version: Some("7".into()),
                }),
                pod: None,
                restart_image: None,
                restart_runtime_workspace: None,
            })
        }
        async fn capture_builder_control(
            &self,
            context: &UserAppExecutionContext,
        ) -> ContainerRuntimeResult<BuilderControlTarget> {
            self.inspect_builder_candidate(context).await
        }
    }
    #[async_trait]
    impl WorkspaceRuntime for ReadOnlyRuntime {}
    #[async_trait]
    impl UserAppDeploymentRuntime for ReadOnlyRuntime {
        async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
            Ok(vec![])
        }

        async fn discover_application_identity(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
            Ok(None)
        }
        async fn prepare_compute_operation(
            &self,
            _: &UserAppExecutionContext,
            _: UserAppOperationScope,
        ) -> ContainerRuntimeResult<Box<dyn shared_types::PreparedComputeLease>> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Err(ContainerRuntimeError::ConfigurationError(
                "unexpected compute preparation".into(),
            ))
        }
    }

    struct Fixture {
        state: Arc<crate::app_state::AppState>,
        store: Arc<TursoUserAppStore>,
        runtime: Arc<ReadOnlyRuntime>,
        _root: tempfile::TempDir,
    }
    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().expect("fixture root");
            let store = Arc::new(
                TursoUserAppStore::open_exclusive(&root.path().join("control.db"))
                    .await
                    .expect("real Turso"),
            );
            store
                .ensure_identity("routeapp")
                .await
                .expect("active lifecycle");
            let runtime = Arc::new(ReadOnlyRuntime::default());
            let activity = Arc::new(app_manager::AppActivityRegistry::new(
                std::time::Duration::from_secs(300),
            ));
            let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
                app_manager::service::AppService::new(
                    app_manager::config::AppManagerConfig {
                        access_mode: app_manager::config::AppAccessMode::Docker,
                        ..Default::default()
                    },
                    runtime.clone(),
                    activity.clone(),
                    None,
                    store.clone(),
                )
                .await
                .expect("real AppService"),
            );
            let (adapter, _cleanup_rx) =
                crate::storage::ProjectAdapter::new("test-ns".into(), "cluster.local".into());
            let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
            let state = Arc::new(crate::app_state::AppState {
                userapp_store: store.clone(),
                userapp_store_control: store.clone(),
                userapp_op_flight: Arc::new(Default::default()),
                userapp_recovery_handle: Arc::new(std::sync::Mutex::new(None)),
                config: crate::config::AppConfig::default(),
                projects: Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
                    adapter,
                ))),
                pingora_service: None,
                userapp_error_page: None,
                grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
                session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
                api_key_config: Arc::new(ArcSwap::from_pointee(
                    crate::config::ApiKeyAuthConfig::default(),
                )),
                pod_creating: Arc::new(dashmap::DashMap::new()),
                pod_created_tx: Arc::new(pod_created_tx),
                container_prefix_rcoder: "fixture-web".into(),
                container_prefix_computer: "fixture-computer".into(),
                runtime: runtime.clone(),
                cleanup_rx: Arc::new(std::sync::Mutex::new(None)),
                agent_download_manager: Arc::new(
                    agent_provisioning::AgentDownloadManager::new(root.path()).expect("downloads"),
                ),
                app_service,
                activity,
                cluster_domain: "cluster.local".into(),
            });
            Self {
                state,
                store,
                runtime,
                _root: root,
            }
        }
        async fn app(&self) -> UserAppLifecycleRecord {
            self.store
                .get_application("routeapp")
                .await
                .expect("read")
                .expect("known app")
        }
        async fn snapshot(
            &self,
        ) -> (
            UserAppLifecycleRecord,
            UserAppComputeStatus,
            Vec<ComputeControlRecord>,
        ) {
            let app = self.app().await;
            let status = self
                .store
                .read_compute_status("routeapp", &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .expect("intent snapshot");
            let active = self
                .store
                .active_compute_controls("routeapp")
                .await
                .expect("active control snapshot");
            (app, status, active)
        }
        async fn seed_stop(&self) -> ComputeControlRecord {
            let app = self.app().await;
            self.store
                .admit_compute_control(&ComputeControlRequest {
                    app_id: "routeapp".into(),
                    lifecycle_id: app.lifecycle_id,
                    scope: UserAppOperationScope::Dev,
                    action: ComputeControlAction::Stop,
                    request_id: "original-stop-request".into(),
                    operation_id: "original-stop-operation".into(),
                    request_fingerprint: "a".repeat(64),
                    restart_image_roll: false,
                })
                .await
                .expect("real durable original Stop")
        }
        async fn request(
            &self,
            route: &str,
            lifecycle_id: &str,
            request_id: &str,
        ) -> (StatusCode, serde_json::Value) {
            let router = Router::new()
                .route("/computer/pod/restart", post(pod_restart))
                .route("/computer/pod/stop", post(pod_stop))
                .with_state(self.state.clone());
            let response = router
                .oneshot(
                    Request::post(route)
                        .header("content-type", "application/json")
                        .header("accept-language", "en-US")
                        .body(Body::from(
                            serde_json::json!({
                                "app_id": "routeapp", "app_stage": "dev", "service_type": "userapp",
                                "lifecycle_id": lifecycle_id, "request_id": request_id,
                            })
                            .to_string(),
                        ))
                        .expect("request"),
                )
                .await
                .expect("actual handler response");
            let status = response.status();
            let body = to_bytes(response.into_body(), 16 * 1024)
                .await
                .expect("body");
            (
                status,
                serde_json::from_slice(&body).expect("JSON envelope"),
            )
        }
        async fn unchanged(
            &self,
            before: &(
                UserAppLifecycleRecord,
                UserAppComputeStatus,
                Vec<ComputeControlRecord>,
            ),
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(
                &self.snapshot().await,
                before,
                "rejected input must not admit or alter an intent"
            );
            assert_eq!(
                self.runtime.writes.load(Ordering::SeqCst),
                0,
                "rejection must not dispatch physical writes"
            );
        }
    }

    #[tokio::test]
    async fn compute_route_wrong_lifecycle_preserves_conflict_without_admission() {
        for route in ["/computer/pod/restart", "/computer/pod/stop"] {
            let fixture = Fixture::new().await;
            let before = fixture.snapshot().await;
            let (status, body) = fixture
                .request(
                    route,
                    "other-valid-lifecycle",
                    "new-invalid-lifecycle-request",
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], shared_types::ERR_CONFLICT);
            assert_eq!(body["success"], false);
            assert!(body.get("operation_id").is_none());
            fixture.unchanged(&before).await;
            fixture.store.shutdown().await.expect("close");
        }
    }

    #[tokio::test]
    async fn compute_route_reused_stop_request_for_restart_is_a_narrow_replay_conflict() {
        let fixture = Fixture::new().await;
        let original = fixture.seed_stop().await;
        let before = fixture.snapshot().await;
        let (status, body) = fixture
            .request(
                "/computer/pod/restart",
                &original.lifecycle_id,
                &original.request_id,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], shared_types::ERR_CONFLICT);
        assert_eq!(body["success"], false);
        assert!(body.get("operation_id").is_none());
        fixture.unchanged(&before).await;
        assert_eq!(
            fixture
                .store
                .get_compute_control("routeapp", &original.operation_id)
                .await
                .unwrap(),
            Some(original)
        );
        fixture.store.shutdown().await.expect("close");
    }

    #[tokio::test]
    async fn compute_route_live_stop_holder_uses_new_busy_code_without_queue_or_new_receipt() {
        let fixture = Fixture::new().await;
        let original = fixture.seed_stop().await;
        let before = fixture.snapshot().await;
        let (status, body) = fixture
            .request(
                "/computer/pod/restart",
                &original.lifecycle_id,
                "different-restart-request",
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], shared_types::ERR_OPERATION_IN_PROGRESS);
        assert_eq!(body["success"], false);
        assert!(
            body.get("operation_id").is_none(),
            "holder is not an accepted request identity"
        );
        assert_eq!(body["blocker"]["operation_id"], original.operation_id);
        assert_eq!(
            body["data"],
            serde_json::json!({
                "holder_operation_id": original.operation_id, "holder_kind": "stop_builder",
                "holder_state": "pending", "holder_step": "accepted", "holder_traffic_wake": false,
                "retryable": false, "retry_after_seconds": 0,
            })
        );
        fixture.unchanged(&before).await;
        fixture.store.shutdown().await.expect("close");
    }

    #[tokio::test]
    async fn compute_route_invalid_reserved_request_is_not_reclassified_as_replay_conflict() {
        let fixture = Fixture::new().await;
        let before = fixture.snapshot().await;
        let (status, body) = fixture
            .request(
                "/computer/pod/stop",
                &before.0.lifecycle_id,
                "auto-repair-reserved-request",
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], shared_types::ERR_INVALID_STATE);
        assert_eq!(body["success"], false);
        assert!(body.get("operation_id").is_none());
        fixture.unchanged(&before).await;
        fixture.store.shutdown().await.expect("close");
    }
}
