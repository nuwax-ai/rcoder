//! 状态检查器状态机回归网（历史上 4 次误杀事故的防线，此前 0 测试）。
//! 锁住的偏移敏感点：
//! - 失败计数升降级与 first_failure_time 只钉首次（窗口锚点漂移 = skip 永期推迟）
//! - skip 窗口的阈值与时长双条件
//! - check_container_exists 按 service_type 的查找键分派（分派轴改错 = 清理错容器）
//! - 健康状态的清理双轴（不在存储 / 超期）

use super::checker::ContainerStatusChecker;
use super::state::{ContainerHealthState, ContainerStatusCheckerConfig};
use crate::config::AppConfig;
use crate::grpc::{GrpcChannelPool, SessionStreamRegistry};
use crate::router::AppState;
use crate::storage::{ProjectAdapter, ProjectStoreBackend};
use agent_provisioning::AgentDownloadManager;
use app_manager::config::{AppAccessMode, AppManagerConfig};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::Utc;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use dashmap::DashMap;
use shared_types::{
    ApiKeyAuthConfig, ContainerBasicInfo, ProjectAndContainerInfo, ProjectExtendedFields,
    ServiceType,
};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;

/// find_container 的可重复响应行为（多次查询返回一致结果）
#[derive(Clone, Copy)]
enum FindBehavior {
    Found,
    Missing,
    Fail,
}

/// 可编程桩运行时：记录 find_container 的每次查询参数，返回行为可由测试预设。
/// 分派语义测试的核心——验证查找键用的是 user_id 还是 project_id。
struct ProbeRuntime {
    behavior: FindBehavior,
    queries: Mutex<Vec<(String, ServiceType)>>,
}

impl ProbeRuntime {
    fn new(behavior: FindBehavior) -> Self {
        Self {
            behavior,
            queries: Mutex::new(Vec::new()),
        }
    }

    fn queried_identifiers(&self) -> Vec<String> {
        self.queries
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[async_trait]
impl AgentContainerRuntime for ProbeRuntime {
    async fn create_container(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ContainerNotFound("probe".into()))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.queries
            .lock()
            .unwrap()
            .push((identifier.to_string(), service_type.clone()));
        match self.behavior {
            FindBehavior::Found => Ok(Some(RuntimeContainerInfo {
                container_id: "stub".to_string(),
                container_name: "stub".to_string(),
                container_ip: "127.0.0.1".to_string(),
                status: ContainerRuntimeStatus::Running,
                created_at: Utc::now(),
                env_vars: None,
            })),
            FindBehavior::Missing => Ok(None),
            FindBehavior::Fail => Err(ContainerRuntimeError::ConnectionError(
                "probe error".to_string(),
            )),
        }
    }
    async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
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
}

// 空 impl 继承默认实现 → ProbeRuntime impl B/C → 自动 impl ContainerRuntime
// 例外：list_deployments 必须返回空列表——AppService::new 构造期会调它
// （rebuild_stopped_apps），默认实现的"not supported"会让构造直接失败
#[async_trait]
impl WorkspaceRuntime for ProbeRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for ProbeRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }
}

/// 轻量 AppState 字面量构造（绕过 AppState::new 的 AppService 装配副作用）。
/// 状态检查器只消费 runtime / grpc_pool / projects 三个字段。
async fn test_state(runtime: Arc<ProbeRuntime>) -> Arc<AppState> {
    let (adapter, _cleanup_rx) =
        ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        300,
    )));
    // 显式 Docker 模式：AppService::new 的 K8s 分支会调 validate_app_prerequisites
    let manager_config = AppManagerConfig {
        access_mode: AppAccessMode::Docker,
        ..AppManagerConfig::default()
    };
    let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
        app_manager::service::AppService::new(
            manager_config,
            runtime.clone(),
            activity.clone(),
            None,
        )
        .await
        .expect("AppService 构造失败"),
    );
    let download_dir = tempfile::tempdir().expect("tempdir");
    let agent_download_manager =
        Arc::new(AgentDownloadManager::new(download_dir.path()).expect("下载管理器构造失败"));
    let (pod_created_tx, _) = broadcast::channel(32);
    Arc::new(AppState {
        config: AppConfig::default(),
        projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
        pingora_service: None,
        grpc_pool: Arc::new(GrpcChannelPool::new()),
        session_stream_registry: Arc::new(SessionStreamRegistry::new()),
        api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
        pod_creating: Arc::new(DashMap::new()),
        pod_created_tx: Arc::new(pod_created_tx),
        container_prefix_rcoder: "dev-rcoder".to_string(),
        container_prefix_computer: "computer-agent-runner".to_string(),
        runtime,
        cleanup_rx: Arc::new(Mutex::new(None)),
        agent_download_manager,
        app_service,
        activity,
        cluster_domain: "cluster.local".to_string(),
    })
}

fn checker(config: ContainerStatusCheckerConfig, state: Arc<AppState>) -> ContainerStatusChecker {
    ContainerStatusChecker::new(config, state)
}

/// 测试载体：可控 service_type / user_id 的容器信息
fn container_info(
    project_id: &str,
    user_id: Option<&str>,
    service_type: ServiceType,
) -> Arc<ProjectAndContainerInfo> {
    let container = ContainerBasicInfo {
        container_id: format!("container_{project_id}"),
        container_name: format!("container_{project_id}"),
        container_ip: "127.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: project_id.to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{project_id}"),
    };
    Arc::new(ProjectAndContainerInfo::from_parts(
        project_id.to_string(),
        user_id.map(str::to_string),
        None,
        None,
        Some(container),
        ProjectExtendedFields {
            service_type: Some(service_type),
            ..Default::default()
        },
    ))
}

#[tokio::test]
async fn record_failure_escalates_and_pins_first_failure_time() {
    let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
    let c = checker(ContainerStatusCheckerConfig::default(), state);

    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));

    let health = c.health_states.get("k").expect("状态条目存在").clone();
    assert_eq!(health.consecutive_failures, 3, "连续失败应累计到阈值");
    let pinned = health.first_failure_time.expect("首次失败时间应被记录");
    // 再失败一次：first_failure_time 不得漂移（skip 窗口锚定首次失败，
    // 若改为每次更新，容器会因间歇失败被永久跳过检查）
    std::thread::sleep(Duration::from_millis(5));
    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
    let health = c.health_states.get("k").expect("状态条目存在").clone();
    assert_eq!(health.consecutive_failures, 4);
    assert_eq!(
        health.first_failure_time.expect("仍有首次时间"),
        pinned,
        "first_failure_time 必须钉在首次失败"
    );
}

#[tokio::test]
async fn record_success_resets_failure_state_completely() {
    let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
    let c = checker(ContainerStatusCheckerConfig::default(), state);

    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
    c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
    c.record_success("k");

    let health = c.health_states.get("k").expect("状态条目存在").clone();
    assert_eq!(health.consecutive_failures, 0, "成功必须清零失败计数");
    assert!(
        health.first_failure_time.is_none(),
        "成功必须清掉 first_failure_time——否则恢复后一次失败即重新进入 skip 窗口语义"
    );
    assert!(health.last_success_time.is_some());
}

#[tokio::test]
async fn should_skip_requires_threshold_and_unexpired_window() {
    let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
    let config = ContainerStatusCheckerConfig {
        failure_threshold: 3,
        skip_duration: Duration::from_secs(300),
        ..Default::default()
    };
    let c = checker(config, state);

    // 未达阈值：不跳过
    c.health_states.insert(
        "below".to_string(),
        ContainerHealthState {
            consecutive_failures: 2,
            first_failure_time: Some(Utc::now()),
            last_check_time: Utc::now(),
            last_success_time: None,
        },
    );
    assert!(!c.should_skip_check("below"), "未达阈值不应跳过");

    // 达阈值且在窗口内：跳过
    c.health_states.insert(
        "inside".to_string(),
        ContainerHealthState {
            consecutive_failures: 3,
            first_failure_time: Some(Utc::now() - chrono::Duration::seconds(60)),
            last_check_time: Utc::now(),
            last_success_time: None,
        },
    );
    assert!(c.should_skip_check("inside"), "达阈值且窗口内应跳过");

    // 窗口已过：恢复检查（容器若被重建，正是靠这条路径自愈）
    c.health_states.insert(
        "expired".to_string(),
        ContainerHealthState {
            consecutive_failures: 5,
            first_failure_time: Some(Utc::now() - chrono::Duration::seconds(301)),
            last_check_time: Utc::now(),
            last_success_time: None,
        },
    );
    assert!(!c.should_skip_check("expired"), "窗口过期必须恢复检查");

    // 无条目：不跳过
    assert!(!c.should_skip_check("absent"));
}

#[tokio::test]
async fn check_container_exists_dispatches_lookup_key_by_service_type() {
    let probe = Arc::new(ProbeRuntime::new(FindBehavior::Found));
    let state = test_state(probe.clone()).await;
    let c = checker(ContainerStatusCheckerConfig::default(), state);

    // ComputerAgentRunner：用 user_id 查
    let info = container_info("proj-1", Some("user-1"), ServiceType::ComputerAgentRunner);
    assert!(c.check_container_exists(&info, "addr").await);
    assert_eq!(
        probe.queried_identifiers(),
        vec!["user-1".to_string()],
        "ComputerAgentRunner 必须以 user_id 为查找键"
    );

    // WebAgentRunner：用 project_id 查
    let info = container_info("proj-2", Some("user-2"), ServiceType::WebAgentRunner);
    assert!(c.check_container_exists(&info, "addr").await);
    assert_eq!(
        probe.queried_identifiers().last().unwrap(),
        "proj-2",
        "WebAgentRunner 必须以 project_id 为查找键"
    );

    // UserappBuilder：复用 project_id 路径
    let info = container_info("proj-3", Some("user-3"), ServiceType::UserappBuilder);
    assert!(c.check_container_exists(&info, "addr").await);
    assert_eq!(
        probe.queried_identifiers().last().unwrap(),
        "proj-3",
        "UserappBuilder 必须以 project_id 为查找键"
    );

    // Userapp：不查 runtime，恒视为不存在（由 app_manager 独立管理）
    let before = probe.queried_identifiers().len();
    let info = container_info("proj-4", Some("user-4"), ServiceType::Userapp);
    assert!(
        !c.check_container_exists(&info, "addr").await,
        "Userapp 恒视为不存在"
    );
    assert_eq!(
        probe.queried_identifiers().len(),
        before,
        "Userapp 分支不得触碰 runtime 查询"
    );
}

#[tokio::test]
async fn check_container_exists_treats_error_and_missing_user_id_as_absent() {
    // 查询 Err：保守视为不存在（触发连接清理路径）
    let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Fail))).await;
    let c = checker(ContainerStatusCheckerConfig::default(), state);
    let info = container_info("proj", Some("user"), ServiceType::ComputerAgentRunner);
    assert!(!c.check_container_exists(&info, "addr").await);

    // ComputerAgentRunner 缺 user_id：视为不存在且不查
    let probe = Arc::new(ProbeRuntime::new(FindBehavior::Found));
    let state = test_state(probe.clone()).await;
    let c = checker(ContainerStatusCheckerConfig::default(), state);
    let info = container_info("proj", None, ServiceType::ComputerAgentRunner);
    assert!(!c.check_container_exists(&info, "addr").await);
    assert!(probe.queried_identifiers().is_empty());
}

#[tokio::test]
async fn cleanup_stale_health_states_removes_unknown_or_expired_only() {
    let probe = Arc::new(ProbeRuntime::new(FindBehavior::Missing));
    let state = test_state(probe).await;
    let config = ContainerStatusCheckerConfig {
        health_reset_interval: Duration::from_secs(1800),
        ..Default::default()
    };
    let c = checker(config, state.clone());

    // 条目 A：不在 projects 存储 → 移除
    c.health_states
        .insert("unknown".to_string(), ContainerHealthState::new());
    // 条目 B：在存储且新近检查 → 保留
    state
        .insert_project(
            "known".to_string(),
            container_info("known", Some("u"), ServiceType::WebAgentRunner),
        )
        .expect("插入 known");
    c.health_states
        .insert("known".to_string(), ContainerHealthState::new());
    // 条目 C：在存储但 last_check 超 reset 周期 → 移除
    state
        .insert_project(
            "stale".to_string(),
            container_info("stale", Some("u"), ServiceType::WebAgentRunner),
        )
        .expect("插入 stale");
    c.health_states.insert(
        "stale".to_string(),
        ContainerHealthState {
            consecutive_failures: 1,
            first_failure_time: None,
            last_check_time: Utc::now() - chrono::Duration::seconds(1801),
            last_success_time: None,
        },
    );

    c.cleanup_stale_health_states();

    assert!(
        c.health_states.get("unknown").is_none(),
        "不在存储的条目应被清理"
    );
    assert!(
        c.health_states.get("known").is_some(),
        "在存储且未过期的条目必须保留"
    );
    assert!(c.health_states.get("stale").is_none(), "超期条目应被清理");
}
