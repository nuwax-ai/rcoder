use std::sync::Arc;

use super::*;
use crate::test_support::{MockRuntime, test_service};

/// dev query 依赖 locator 做 builder 在跑探测——stub 恒"在"（非 orphan）
struct StubDevLocator;
#[async_trait::async_trait]
impl shared_types::UserappDevLocator for StubDevLocator {
    async fn dev_file_server_addr(&self, _app_id: &str) -> Result<String, String> {
        Ok("http://127.0.0.1:60000".to_string())
    }
    async fn dev_container_alive(&self, _app_id: &str) -> Result<bool, String> {
        Ok(true)
    }
}

/// storage 的 app_stage 分派落点：workspace_volume_name / list_workspace_identifiers
/// 必须按 app_stage 换 ServiceType（dev→UserappBuilder / prod→Userapp）——K8s 卷
/// label 与 Docker 目录树都按它分形，分派错即查错卷。
#[tokio::test]
async fn storage_env_dispatches_service_type() {
    let runtime = Arc::new(MockRuntime::default());
    let root = tempfile::tempdir().expect("test root");
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    for app in ["app1", "appdev", "appprod"] {
        service
            .metadata
            .record(app, None, None, None)
            .await
            .expect("owner");
    }

    service
        .metadata
        .record("app1", None, None, None)
        .await
        .expect("register owner");
    service
        .get_app_storage(UserappStage::Prod, "app1")
        .await
        .expect("prod storage");
    service
        .get_app_storage(UserappStage::Dev, "app1")
        .await
        .expect("dev storage");

    let calls = runtime.volume_name_calls.get("app1").expect("calls");
    assert_eq!(
        *calls,
        vec!["Userapp".to_string(), "UserappBuilder".to_string()],
        "prod 先查运行卷、dev 查开发卷（ServiceType 分派）"
    );
}

/// query 的 app_stage 分派：dev 清单枚举 UserappBuilder 卷（不并入 Deployment 集），
/// prod 枚举 Userapp 卷（并入运行中 app 兜底）。
#[tokio::test]
async fn query_storage_env_selects_volume_family() {
    let runtime = Arc::new(MockRuntime::default());
    runtime
        .workspace_ids
        .insert("UserappBuilder".to_string(), vec!["appdev".to_string()]);
    runtime
        .workspace_ids
        .insert("Userapp".to_string(), vec!["appprod".to_string()]);
    let root = tempfile::tempdir().expect("test root");
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    for app in ["app1", "appdev", "appprod"] {
        service
            .metadata
            .record(app, None, None, None)
            .await
            .expect("owner");
    }
    *service.dev_locator.write().expect("dev_locator lock") = Some(Arc::new(StubDevLocator));

    let dev_resp = service
        .query_storage(
            UserappStage::Dev,
            QueryStorageRequest {
                page: 1,
                page_size: 10,
                filters: None,
            },
        )
        .await
        .expect("dev query");
    assert_eq!(
        dev_resp
            .items
            .iter()
            .map(|i| i.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["appdev"],
        "dev 清单只含开发卷"
    );

    let prod_resp = service
        .query_storage(
            UserappStage::Prod,
            QueryStorageRequest {
                page: 1,
                page_size: 10,
                filters: None,
            },
        )
        .await
        .expect("prod query");
    assert_eq!(
        prod_resp
            .items
            .iter()
            .map(|i| i.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["appprod"],
        "prod 清单只含运行卷"
    );
}

#[derive(Clone)]
struct ClearCredentials {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    fails: bool,
}
#[async_trait::async_trait]
impl shared_types::FileServerCredentialsProvider for ClearCredentials {
    async fn for_target(
        &self,
        stage: UserappStage,
        _: &str,
        _: tokio::time::Instant,
    ) -> Result<shared_types::FileServerRequestCredentials, shared_types::WakeFailure> {
        assert_eq!(stage, UserappStage::Dev);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fails {
            Err(shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_CONFIGURATION,
                "file_credentials_configuration",
                "Existing file credential configuration is invalid",
            ))
        } else {
            Ok(shared_types::FileServerRequestCredentials {
                proxy_token: Some("fixture-clear-token".into()),
            })
        }
    }
}
#[derive(Clone, Default)]
struct ClearCounters {
    captured: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<std::sync::atomic::AtomicUsize>,
}
struct ClearTicket {
    app_id: String,
    counters: ClearCounters,
}
#[async_trait::async_trait]
impl shared_types::UserappDevDeletion for ClearTicket {
    fn receipt(&self) -> shared_types::UserappDevDeletionReceipt {
        let mut receipt = crate::test_support::dev_deletion_receipt(&self.app_id);
        receipt.runtime.docker_bind_cleanup = true;
        receipt
    }
    async fn workspace_endpoint(
        &mut self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint, String> {
        if context.app_id != self.app_id {
            return Err("Captured physical target belongs to another app".into());
        }
        Ok(shared_types::UserAppBuilderWorkspaceEndpoint {
            container_id: "fixture-builder-uid".into(),
            address: std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        })
    }
    fn begin_external_mutation(&mut self) -> Result<(), String> {
        self.counters
            .started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn finish_external_mutation(&mut self) -> Result<(), String> {
        self.counters
            .finished
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn cleanup(self: Box<Self>) -> Result<(), String> {
        Err("Clear must retain builder compute".into())
    }
}
#[async_trait::async_trait]
impl shared_types::UserappDevCleanup for ClearCounters {
    async fn capture(
        &self,
        app_id: &str,
    ) -> Result<Box<dyn shared_types::UserappDevDeletion>, String> {
        self.captured
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(ClearTicket {
            app_id: app_id.into(),
            counters: self.clone(),
        }))
    }
}

#[tokio::test]
async fn clear_uses_one_credential_snapshot_for_probe_and_write_without_deleting_compute() {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let http_calls = Arc::new(AtomicUsize::new(0));
    let probe_calls = http_calls.clone();
    let write_calls = http_calls.clone();
    let router = Router::new()
        .route("/api/v1/userapp/app-files/clear-target", get(move |headers:HeaderMap| {
            let calls = probe_calls.clone();
            async move {
                assert!(!headers.contains_key("x-api-key"));
                if headers.get("x-proxy-token").and_then(|v|v.to_str().ok()) != Some("fixture-clear-token") {
                    return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"success":false})));
                }
                calls.fetch_add(1,Ordering::SeqCst);
                (StatusCode::OK,Json(serde_json::json!({"app_id":"fixtureapp","instance_id":"captured-file-process"})))
            }
        }))
        .route("/api/v1/userapp/app-files/clear", post(move |headers:HeaderMap, Json(request):Json<shared_types::UserAppWorkspaceClearRequest>| {
            let calls = write_calls.clone();
            async move {
                assert!(!headers.contains_key("x-api-key"));
                if headers.get("x-proxy-token").and_then(|v|v.to_str().ok()) != Some("fixture-clear-token") {
                    return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"success":false})));
                }
                assert_eq!(request.app_id,"fixtureapp");
                assert_eq!(request.expected_instance_id,"captured-file-process");
                calls.fetch_add(1,Ordering::SeqCst);
                (StatusCode::OK,Json(serde_json::json!({"success":true,"instance_id":"captured-file-process"})))
            }
        }));
    let listener = tokio::net::TcpListener::bind((
        std::net::Ipv6Addr::LOCALHOST,
        shared_types::AGENT_FILE_SERVER_PORT,
    ))
    .await
    .expect("isolated loopback file peer at the actual fixed port");
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("fixture server");
    });
    let root = tempfile::tempdir().expect("root");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .record("fixtureapp", None, None, None)
        .await
        .expect("application identity");
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    let counters = ClearCounters::default();
    service
        .set_dev_cleanup(Arc::new(counters.clone()))
        .expect("physical ticket provider");
    let credential_calls = Arc::new(AtomicUsize::new(0));
    service
        .set_file_credentials_provider(Arc::new(ClearCredentials {
            calls: credential_calls.clone(),
            fails: false,
        }))
        .expect("credential provider");
    let result = service
        .clear_app_storage(UserappStage::Dev, "fixtureapp")
        .await;
    server.abort();
    drop(server.await);
    result.expect("actual guarded clear succeeds");
    assert_eq!(
        credential_calls.load(Ordering::SeqCst),
        1,
        "one immutable credential snapshot"
    );
    assert_eq!(
        http_calls.load(Ordering::SeqCst),
        2,
        "probe then exactly one write"
    );
    assert_eq!(counters.captured.load(Ordering::SeqCst), 1);
    assert_eq!(counters.started.load(Ordering::SeqCst), 1);
    assert_eq!(
        counters.finished.load(Ordering::SeqCst),
        1,
        "actual acknowledgement releases mutation"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn clear_credential_failure_precedes_physical_capture_and_external_mutation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().expect("root");
    let service = test_service(root.path(), Arc::default()).await;
    service
        .metadata
        .record("fixtureapp", None, None, None)
        .await
        .expect("application identity");
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    let counters = ClearCounters::default();
    service
        .set_dev_cleanup(Arc::new(counters.clone()))
        .expect("physical ticket provider");
    let credential_calls = Arc::new(AtomicUsize::new(0));
    service
        .set_file_credentials_provider(Arc::new(ClearCredentials {
            calls: credential_calls.clone(),
            fails: true,
        }))
        .expect("credential provider");
    let error = service
        .clear_app_storage(UserappStage::Dev, "fixtureapp")
        .await
        .expect_err("invalid configuration");
    assert_eq!(error.code(), shared_types::ERR_RUNTIME_CONFIGURATION);
    assert_eq!(credential_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        counters.captured.load(Ordering::SeqCst),
        0,
        "configuration failure precedes physical capture"
    );
    assert_eq!(counters.started.load(Ordering::SeqCst), 0);
    assert_eq!(counters.finished.load(Ordering::SeqCst), 0);
    assert!(!error.requires_recovery(), "no external write dispatched");
}

struct ClearChildCredentialFailure {
    direct_child: bool,
}
#[async_trait::async_trait]
impl shared_types::FileServerCredentialsProvider for ClearChildCredentialFailure {
    async fn for_target(
        &self,
        stage: UserappStage,
        app_id: &str,
        _: tokio::time::Instant,
    ) -> Result<shared_types::FileServerRequestCredentials, shared_types::WakeFailure> {
        assert_eq!(stage, UserappStage::Dev);
        assert_eq!(app_id, "fixtureapp");
        Err(shared_types::WakeFailure {
            operation_id: self
                .direct_child
                .then(|| "original-child-credential-read".into()),
            command_diagnostic: Some(Box::new(shared_types::PgCommandDiagnostic {
                code: shared_types::ERR_RUNTIME_CONFIGURATION.into(),
                operation_id: Some("original-child-credential-read".into()),
                blocker: Some(shared_types::UserAppOperationBlocker {
                    scope: shared_types::UserAppOperationScope::Dev,
                    operation_id: "original-blocking-file-stop".into(),
                    kind: shared_types::UserAppOperationKind::StopBuilder,
                    state: shared_types::UserAppOperationState::Running,
                    step: "stop".into(),
                }),
                error_detail: Some(
                    shared_types::ErrorDetail::new(
                        shared_types::ERR_RUNTIME_CONFIGURATION,
                        "original_credential_read",
                        "File credential configuration was rejected",
                    )
                    .with_task_id("original-child-credential-task"),
                ),
            })),
            ..shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_CONFIGURATION,
                "file_credentials_configuration",
                "File credential configuration was rejected",
            )
        })
    }
}
#[tokio::test]
async fn admitted_clear_error_keeps_captured_parent_identity_and_original_child_task() {
    for direct_child in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime.clone()).await;
        service
            .metadata
            .record("fixtureapp", None, None, None)
            .await
            .unwrap();
        service.set_dev_locator(Arc::new(StubDevLocator)).unwrap();
        let counters = ClearCounters::default();
        service.set_dev_cleanup(Arc::new(counters.clone())).unwrap();
        service
            .set_file_credentials_provider(Arc::new(ClearChildCredentialFailure { direct_child }))
            .unwrap();
        let request_id = "captured-clear-parent-request";
        let error = service
            .clear_app_storage_controlled(
                UserappStage::Dev,
                "fixtureapp",
                ClearStorageRequest {
                    lifecycle_id: None,
                    request_id: Some(request_id.into()),
                },
            )
            .await
            .unwrap_err();
        let admitted = service
            .metadata
            .store
            .get_operation_by_request("fixtureapp", request_id)
            .await
            .unwrap()
            .expect("actual durable clear admission receipt");
        assert_eq!(
            admitted.state,
            shared_types::UserAppOperationState::Failed,
            "known pre-mutation failure is not an unknown write"
        );
        assert_eq!(
            counters.captured.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            counters.started.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            runtime
                .delete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(!error.requires_recovery());
        let response = shared_types::AppError::from(error).into_http_result::<()>("en-US");
        assert_eq!(response.code, shared_types::ERR_RUNTIME_CONFIGURATION);
        assert_eq!(
            response.operation_id.as_deref(),
            Some(admitted.operation_id.as_str()),
            "captured parent identity must override later child observation"
        );
        let detail = response.error_detail.expect("original child diagnostic");
        assert_eq!(
            detail.task_id.as_deref(),
            Some("original-child-credential-task")
        );
        assert_eq!(detail.stage, "original_credential_read");
        assert_eq!(
            response.blocker.unwrap().operation_id,
            "original-blocking-file-stop"
        );
    }
}
