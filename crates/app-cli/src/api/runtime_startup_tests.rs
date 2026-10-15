use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::Value;

use crate::runtime_kernel::{RuntimeKernel, RuntimeStore};
use crate::runtime_status::RuntimeStatusService;
use crate::server::ServerState;

struct ManagementApi {
    address: String,
    driver: tokio::task::JoinHandle<()>,
}

impl ManagementApi {
    async fn start(root: &std::path::Path, state: Arc<ServerState>) -> Self {
        let (listener, router) = crate::api::bind(
            "127.0.0.1:0",
            root.join("workspace"),
            root.join("logs"),
            root.join("unused-pingap"),
            state,
        )
        .await
        .unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let driver = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { address, driver }
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}{path}", self.address))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap())
    }
}

impl Drop for ManagementApi {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

fn kernel(root: &std::path::Path) -> Arc<RuntimeKernel> {
    let source = root.join("workspace");
    std::fs::create_dir_all(&source).unwrap();
    let store = RuntimeStore::open_with_root(root.join("control"), &source).unwrap();
    let identity = store
        .load_or_init_identity(
            "startup-app".into(),
            "userapp-dev".into(),
            "workspace".into(),
            source.to_string_lossy().into_owned(),
            "startup-generation".into(),
        )
        .unwrap();
    Arc::new(RuntimeKernel::new(
        store,
        identity,
        Box::new(|_| panic!("read-only startup observations must not dispatch business")),
    ))
}

#[tokio::test]
async fn pending_owner_kernel_assembly_reports_initializing_through_bound_router() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.initialize_owner_token().unwrap();
    state.mark_kernel_required();
    let api = ManagementApi::start(root.path(), state.clone()).await;
    let prepared = kernel(root.path());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let assembly_state = state.clone();
    let assembly_kernel = prepared.clone();
    let assembly = tokio::spawn(async move {
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        assembly_kernel
            .store()
            .store_token(&assembly_state.control_token().unwrap())
            .unwrap();
        assert!(assembly_state.set_runtime_kernel(assembly_kernel));
    });
    entered_rx.await.unwrap();

    for path in ["/v1/runtime/identity", "/v1/runtime/status"] {
        let (status, body) = api.get(path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert_eq!(body["code"], "ERR_INITIALIZING", "{path}: {body}");
        assert_eq!(body["success"], false);
    }
    assert!(
        prepared
            .status()
            .await
            .unwrap()
            .active_operation_id
            .is_none()
    );
    assert_eq!(state.pending_deploy_count().await, 0);
    assert!(state.runtime_kernel().is_none());

    release_tx.send(()).unwrap();
    assembly.await.unwrap();
    // Kernel availability is independent of the later business recovery gate.
    assert!(state.initializing());
    for path in ["/v1/runtime/identity", "/v1/runtime/status"] {
        let (status, body) = api.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["success"], true);
        assert_eq!(
            body["data"]["runtime_instance_id"],
            prepared.identity().runtime_instance_id
        );
    }
    assert!(
        prepared
            .status()
            .await
            .unwrap()
            .active_operation_id
            .is_none()
    );
    assert_eq!(state.pending_deploy_count().await, 0);
}
