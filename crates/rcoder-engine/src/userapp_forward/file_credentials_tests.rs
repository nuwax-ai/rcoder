//! Dev 凭证不再读取运行时进程（fd-scan 已删，见 file_credentials.rs 注释）：
//! 仅透传平台静态配置。PeerRuntime 的方法全部保持 panic!，证明 credentials(Dev)
//! 不触运行时、不触 exec。
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeResult, RuntimeContainerInfo,
    UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::sync::Arc;

struct PeerRuntime;

#[async_trait::async_trait]
impl AgentContainerRuntime for PeerRuntime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("dev credentials must not create compute")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("dev credentials must not use unbound discovery")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        panic!("dev credentials must not discover compute")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("dev credentials must not stop compute")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("dev credentials must not probe business readiness")
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("dev credentials must not clean compute")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for PeerRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for PeerRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }
    async fn get_app_container_spec(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ContainerSpecSnapshot> {
        panic!("dev credentials must not read specs")
    }
}

async fn state_with_configured_token(token: Option<&str>) -> (Arc<AppState>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("credentials.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(PeerRuntime);
    let mut state = crate::userapp_builder::builder_runtime_test_state(runtime, store, None).await;
    Arc::get_mut(&mut state).unwrap().config.file_server_proxy =
        Some(file_server_proxy::FileServerProxyConfig {
            auth_token: token.map(str::to_string),
            ..Default::default()
        });
    (state, directory)
}

#[tokio::test]
async fn dev_credentials_never_inherit_main_entry_token() {
    // 主入口 file_server_proxy.auth_token 配置了 token 时，dev builder 链路
    // 仍恒定无凭据（codex 复审定案：两套认证边界不得混同）。
    let (state, _directory) = state_with_configured_token(Some("platform-static-token")).await;
    let actual = credentials(
        &state,
        UserappStage::Dev,
        "fixtureapp",
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(actual.proxy_token, None);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn dev_credentials_without_configuration_are_tokenless() {
    let (state, _directory) = state_with_configured_token(None).await;
    let actual = credentials(
        &state,
        UserappStage::Dev,
        "fixtureapp",
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(actual.proxy_token, None);
    state.userapp_store_control.shutdown().await.unwrap();
}
