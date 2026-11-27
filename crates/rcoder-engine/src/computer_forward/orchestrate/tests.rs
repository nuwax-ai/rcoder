//! 迁移编排的用例。
//!
//! 前半部分是纯决策函数；后半部分是补迁编排对着可编程的运行时桩跑——停 agent、起 Job、
//! 恢复 agent 的顺序, 以及"Job 失败之后 agent 必须被拉回来"（编排最严重的事故形态是
//! 用户的 agent 永久停在 0 副本）。

use super::{AgentRestore, ServiceResourceLimits, require_limits_when_rebuilding, wrap};
use container_runtime_api::ContainerRuntimeError;

#[test]
fn idle_user_needs_no_limits_to_be_left_stopped() {
    assert!(require_limits_when_rebuilding("u1", false, None).is_ok());
}

#[test]
fn running_user_with_limits_may_be_stopped_and_rebuilt() {
    let limits = ServiceResourceLimits::default();
    assert!(require_limits_when_rebuilding("u1", true, Some(&limits)).is_ok());
}

/// 反例：切换前 agent 在运行、限额配置缺失——必须在停 agent **之前**拒绝。
/// 停了起不来（wake 路径对同一缺失同样拒绝），只会留下停机。
#[test]
fn running_user_without_limits_is_refused_before_any_stop() {
    let error = require_limits_when_rebuilding("u1", true, None).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("resource_limits"),
        "错误要点名缺的是哪项配置: {message}"
    );
    assert!(message.contains("'u1'"), "错误要点名用户: {message}");
    assert!(
        matches!(error, ContainerRuntimeError::Conflict(_)),
        "配置缺失是确定性拒绝: {error:?}"
    );
}

/// 反例：`wrap` 曾把所有错误压成 K8sError——"门禁被占、稍后重试"到 HTTP 层
/// 变成 500，调用方分不清该重试还是该改配置。
#[test]
fn wrap_keeps_the_variants_that_http_status_depends_on() {
    let preserved = [
        (
            ContainerRuntimeError::Conflict("already migrated".to_string()),
            "already migrated",
            "must stay Conflict",
        ),
        (
            ContainerRuntimeError::ConnectionError("gate held; retry shortly".to_string()),
            "gate held; retry shortly",
            "must stay ConnectionError",
        ),
        (
            ContainerRuntimeError::ConfigurationError("image not configured".to_string()),
            "image not configured",
            "must stay ConfigurationError",
        ),
    ];
    for (error, payload, expectation) in preserved {
        let before = format!("{error:?}");
        let wrapped = wrap("migration_gate", error);
        let after = format!("{wrapped:?}");
        assert!(
            after.starts_with(before.split('(').next().unwrap_or_default()),
            "{expectation}: {before} -> {after}"
        );
        assert!(
            after.contains(&format!(
                "migration step 'migration_gate' failed: {payload}"
            )),
            "步骤名要带进去: {after}"
        );
    }

    // 自带 HTTP 语义、载荷不是字符串的变体原样透传。agent 正在启动（StatefulSet
    // 在、Pod 还没就绪）时 find_container 会给出前两个；此前被压成 K8sError，
    // 可重试的状态变成了 500。
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::ManagementNotRunning
        ),
        ContainerRuntimeError::ManagementNotRunning
    ));
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::OperationInProgress(Box::new(
                shared_types::UserAppOperationInProgress {
                    app_id: "app-1".to_string(),
                    service_type: shared_types::ServiceType::ComputerAgentRunner,
                    resource_name: "rcoder-computer-agent-runner-u1".to_string(),
                    operation_id: None,
                }
            ))
        ),
        ContainerRuntimeError::OperationInProgress(_)
    ));
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::RequestRejected(shared_types::RuntimeRequestRejection {
                status: 409,
                message: "busy".to_string(),
            })
        ),
        ContainerRuntimeError::RequestRejected(ref rejection) if rejection.status == 409
    ));

    // 上游失败类变体仍归并为 K8sError（没有更具体的 HTTP 语义），文案不变
    let error = wrap(
        "stop_agent",
        ContainerRuntimeError::ContainerStopError("boom".to_string()),
    );
    let ContainerRuntimeError::K8sError(message) = error else {
        panic!("上游失败应归并为 K8sError，实际是 {error:?}");
    };
    assert_eq!(
        message,
        "migration step 'stop_agent' failed: Container stop failed: boom"
    );
}

/// 反例（部署评审）：编排锁冲突是"稍后重试"，AppError 的状态码是 503；此前
/// 投影成 K8sError，到 HTTP 层变成 500，与文档承诺的 503 不符。
#[test]
fn app_errors_keep_their_http_semantics_when_projected() {
    use crate::AppError;
    use axum::http::StatusCode;
    use shared_types::error_codes::*;

    let cases = [
        // 锁冲突："稍后重试"
        (
            AppError::with_message(
                ERR_CONTAINER_ADDRESS_NOT_READY,
                "Another request is still ensuring this container; retry later",
            ),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            AppError::with_message(ERR_OPERATION_IN_PROGRESS, "blocked"),
            StatusCode::CONFLICT,
        ),
        (
            AppError::with_message(ERR_CONTAINER_NOT_FOUND, "gone"),
            StatusCode::NOT_FOUND,
        ),
        (
            AppError::with_message(ERR_RUNTIME_TIMEOUT, "slow"),
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (
            AppError::with_message(ERR_BACKEND_ERROR, "upstream"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ];
    for (error, expected) in cases {
        let own_status = error.status_code();
        let projected = super::runtime_error_from_app(error);
        let status = shared_types::error_status_from_code(
            container_runtime_api::runtime_error_code(&projected).0,
        );
        assert_eq!(
            status, own_status,
            "AppError 自己的状态码要原样带到 HTTP 层: {projected:?}"
        );
        assert_eq!(status, expected, "{projected:?}");
    }

    // 运行时错误里没有 400 的对应变体：对不上的维持 K8sError（500）
    let projected =
        super::runtime_error_from_app(AppError::with_message(ERR_VALIDATION, "bad input"));
    assert!(
        matches!(projected, ContainerRuntimeError::K8sError(_)),
        "{projected:?}"
    );
}

#[test]
fn running_agent_is_restarted_whatever_the_later_observation() {
    assert_eq!(AgentRestore::decide(true, false), AgentRestore::Restart);
    assert_eq!(AgentRestore::decide(true, true), AgentRestore::Restart);
}

#[test]
fn idle_agent_stays_stopped() {
    assert_eq!(
        AgentRestore::decide(false, false),
        AgentRestore::KeepStopped
    );
}

#[test]
fn agent_woken_during_cutover_is_replaced() {
    assert_eq!(
        AgentRestore::decide(false, true),
        AgentRestore::ReplaceWoken
    );
}

/// 补迁编排：运行时桩的迁移相关行为由测试预设, 每次调用都记进轨迹。
mod backfill_orchestration {
    use super::*;
    use crate::app_state::AppState;
    use crate::computer_forward::orchestrate::backfill;
    use crate::config::AppConfig;
    use crate::grpc::{GrpcChannelPool, SessionStreamRegistry};
    use crate::storage::{ProjectAdapter, ProjectStoreBackend};
    use agent_provisioning::AgentDownloadManager;
    use app_manager::config::{AppAccessMode, AppManagerConfig};
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use chrono::Utc;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeResult,
        ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
    };
    use dashmap::DashMap;
    use shared_types::{
        ApiKeyAuthConfig, ServiceType, WorkspaceBackfill, WorkspaceBackfillReport,
        WorkspaceMigrationState,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::broadcast;

    const USER: &str = "backfill-u1";

    /// 场景 knobs（测试按需打开）。
    #[derive(Default)]
    struct Scenario {
        migrated: bool,
        pvc_exists: bool,
        /// 停之前 agent 在跑（卷被它挂着）。
        agent_running: bool,
        backfill_fails: bool,
    }

    #[derive(Default)]
    struct ProbeState {
        migrated: AtomicBool,
        pvc_exists: AtomicBool,
        attached: AtomicBool,
        agent_present: AtomicBool,
        backfill_fails: AtomicBool,
        /// 运行时被调用的轨迹。
        calls: Mutex<Vec<String>>,
    }

    impl ProbeState {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    struct ProbeRuntime {
        state: Arc<ProbeState>,
    }

    fn running_info() -> shared_types::ContainerBasicInfo {
        shared_types::ContainerBasicInfo {
            container_id: "stub".to_string(),
            container_name: "stub".to_string(),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 0,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: String::new(),
            workload_uid: None,
        }
    }

    fn observed() -> RuntimeContainerInfo {
        RuntimeContainerInfo {
            container_id: "stub".to_string(),
            container_name: "stub".to_string(),
            container_ip: "127.0.0.1".to_string(),
            status: ContainerRuntimeStatus::Running,
            created_at: Utc::now(),
            env_vars: None,
            service_type: None,
            project_id: None,
            user_id: None,
            pod_id: None,
            app_id: None,
            workload_uid: None,
        }
    }

    #[async_trait]
    impl AgentContainerRuntime for ProbeRuntime {
        async fn create_container(
            &self,
            _: ContainerCreateParams,
        ) -> ContainerRuntimeResult<shared_types::ContainerBasicInfo> {
            self.state.record("create".to_string());
            self.state.agent_present.store(true, Ordering::SeqCst);
            Ok(running_info())
        }

        async fn get_container_info(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<shared_types::ContainerBasicInfo>> {
            Ok(None)
        }

        async fn get_container_info_by_identifier(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Option<shared_types::ContainerBasicInfo>> {
            Ok(self
                .state
                .agent_present
                .load(Ordering::SeqCst)
                .then(running_info))
        }

        async fn find_container(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            Ok(self
                .state
                .agent_present
                .load(Ordering::SeqCst)
                .then(observed))
        }

        /// ensure 的观察点：桩在这里模拟"agent 被拉起来了"。
        async fn find_container_for_ensure(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            self.state.record("ensure".to_string());
            self.state.agent_present.store(true, Ordering::SeqCst);
            self.state.attached.store(true, Ordering::SeqCst);
            Ok(Some(observed()))
        }

        async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
            self.state.record("stop".to_string());
            self.state.agent_present.store(false, Ordering::SeqCst);
            self.state.attached.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
            Ok(self.state.agent_present.load(Ordering::SeqCst))
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

    #[async_trait]
    impl WorkspaceRuntime for ProbeRuntime {
        async fn workspace_pvc_exists(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<bool> {
            Ok(self.state.pvc_exists.load(Ordering::SeqCst))
        }

        async fn workspace_migration_state(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<WorkspaceMigrationState> {
            Ok(if self.state.migrated.load(Ordering::SeqCst) {
                WorkspaceMigrationState::Migrated
            } else {
                WorkspaceMigrationState::NotMigrated
            })
        }

        async fn check_workspace_migration_config(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }

        async fn workspace_volume_attached(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<bool> {
            Ok(self.state.attached.load(Ordering::SeqCst))
        }

        async fn backfill_workspace(
            &self,
            _: &str,
            _: &ServiceType,
            backfill: &WorkspaceBackfill,
            src_pvc: &str,
        ) -> ContainerRuntimeResult<WorkspaceBackfillReport> {
            self.state.record(format!(
                "backfill:{src_pvc}:{:?}/{:?}",
                backfill.paths, backfill.only
            ));
            if self.state.backfill_fails.load(Ordering::SeqCst) {
                return Err(ContainerRuntimeError::K8sError(
                    "workspace backfill job 'j' did not succeed (failed=3, active=0): out of space"
                        .to_string(),
                ));
            }
            Ok(WorkspaceBackfillReport {
                copied: backfill.paths.clone(),
                skipped: vec![".nope".to_string()],
            })
        }
    }

    #[async_trait]
    impl UserAppDeploymentRuntime for ProbeRuntime {
        async fn list_deployments(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
            Ok(vec![])
        }
    }

    /// 轻量 AppState 字面量构造（绕过 AppState::new 的 AppService 装配副作用；做法同
    /// container_status_checker 的用例）。限额配置必须给：收尾重建 agent 时要用。
    struct Harness {
        state: Arc<AppState>,
        probe: Arc<ProbeState>,
        /// 库文件、下载目录都在这两个临时目录里：得比 AppState 活得久
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    async fn harness_of(scenario: &Scenario) -> Harness {
        let state = Arc::new(ProbeState::default());
        state.migrated.store(scenario.migrated, Ordering::SeqCst);
        state
            .pvc_exists
            .store(scenario.pvc_exists, Ordering::SeqCst);
        state
            .agent_present
            .store(scenario.agent_running, Ordering::SeqCst);
        state
            .attached
            .store(scenario.agent_running, Ordering::SeqCst);
        state
            .backfill_fails
            .store(scenario.backfill_fails, Ordering::SeqCst);
        let runtime: Arc<dyn container_runtime_api::ContainerRuntime> = Arc::new(ProbeRuntime {
            state: state.clone(),
        });

        let adapter = ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
        let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
            300,
        )));
        // 显式 Docker 模式：AppService::new 的 K8s 分支会调 validate_app_prerequisites
        let manager_config = AppManagerConfig {
            access_mode: AppAccessMode::Docker,
            ..AppManagerConfig::default()
        };
        let metadata_dir = tempfile::tempdir().expect("metadata directory");
        let metadata_store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &metadata_dir.path().join("userapp.turso.db"),
        )
        .await
        .expect("Turso metadata store");
        let metadata_store = Arc::new(metadata_store);
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
            app_manager::service::AppService::new(
                manager_config,
                runtime.clone(),
                activity.clone(),
                None,
                metadata_store.clone(),
            )
            .await
            .expect("AppService 构造失败"),
        );
        let download_dir = tempfile::tempdir().expect("tempdir");
        let agent_download_manager =
            Arc::new(AgentDownloadManager::new(download_dir.path()).expect("下载管理器构造失败"));
        let (pod_created_tx, _) = broadcast::channel(32);
        let kubernetes_config: shared_types::KubernetesConfig =
            serde_json::from_value(serde_json::json!({
                "services": {
                    "computer-agent-runner": {
                        "service_type": "ComputerAgentRunner",
                        "image": "registry.invalid/computer:test",
                        "resource_limits": {"memory": 536870912.0, "cpu": 1.0}
                    }
                }
            }))
            .unwrap();
        let app_state = Arc::new(AppState {
            userapp_store: metadata_store.clone(),
            userapp_store_control: metadata_store,
            userapp_op_flight: Arc::new(
                crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
            ),
            userapp_recovery_handle: Arc::new(Mutex::new(None)),
            config: AppConfig {
                kubernetes_config: Some(kubernetes_config),
                ..Default::default()
            },
            projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
            pingora_service: None,
            userapp_error_page: None,
            grpc_pool: Arc::new(GrpcChannelPool::new()),
            session_stream_registry: Arc::new(SessionStreamRegistry::new()),
            api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
            pod_creating: Arc::new(DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "test-rcoder".to_string(),
            container_prefix_computer: "computer-agent-runner".to_string(),
            runtime,
            agent_download_manager,
            app_service,
            activity,
            cluster_domain: "cluster.local".to_string(),
        });
        Harness {
            state: app_state,
            probe: state,
            _dirs: (metadata_dir, download_dir),
        }
    }

    fn request(paths: &[&str], only: &[&str]) -> WorkspaceBackfill {
        WorkspaceBackfill {
            paths: paths.iter().map(|p| (*p).to_string()).collect(),
            only: only.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    /// 未迁移的用户：数据面还在共享卷上, 往 RBD 卷上补没有意义。必须在停任何东西之前拒绝。
    #[tokio::test]
    async fn a_user_that_is_not_migrated_is_refused_before_anything_is_stopped() {
        let harness = harness_of(&Scenario {
            agent_running: true,
            ..Default::default()
        })
        .await;
        let error = backfill(&harness.state, USER, "shared", &request(&[".local"], &[]))
            .await
            .expect_err("未迁移用户不该被补迁");
        assert!(
            matches!(&error, ContainerRuntimeError::Conflict(message) if message.contains("is not migrated")),
            "{error:?}"
        );
        assert_eq!(
            harness.probe.calls(),
            Vec::<String>::new(),
            "拒绝之前不能有任何副作用"
        );
        assert!(
            harness.probe.agent_present.load(Ordering::SeqCst),
            "agent 不能被停"
        );
    }

    #[tokio::test]
    async fn a_missing_volume_is_reported_before_the_agent_is_stopped() {
        let harness = harness_of(&Scenario {
            migrated: true,
            agent_running: true,
            ..Default::default()
        })
        .await;
        let error = backfill(&harness.state, USER, "shared", &request(&[], &[]))
            .await
            .expect_err("没有卷不该被补迁");
        assert!(
            matches!(&error, ContainerRuntimeError::ContainerNotFound(message) if message.contains("run phase-a")),
            "{error:?}"
        );
        assert_eq!(harness.probe.calls(), Vec::<String>::new());
    }

    /// 请求里的错误同样先于一切 I/O：范围会作为字面量拼进 Job 脚本。
    #[tokio::test]
    async fn an_escaping_scope_is_refused_before_anything_is_touched() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            agent_running: true,
            ..Default::default()
        })
        .await;
        let error = backfill(
            &harness.state,
            USER,
            "shared",
            &request(&["../other-user"], &[]),
        )
        .await
        .expect_err("越界的范围不该被补迁");
        assert!(
            matches!(&error, ContainerRuntimeError::ConfigurationError(message) if message.contains("must not contain")),
            "{error:?}"
        );
        assert_eq!(harness.probe.calls(), Vec::<String>::new());
    }

    /// 在跑的 agent：停 → 等卸载 → 补迁（范围、模式、源 PVC 原样交给 Job）→ 拉起来。标记不变。
    #[tokio::test]
    async fn a_running_agent_is_stopped_backfilled_and_started_again() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            agent_running: true,
            ..Default::default()
        })
        .await;
        let outcome = backfill(
            &harness.state,
            USER,
            "shared-pvc",
            &request(&["*", "proj-a"], &["node_modules/**"]),
        )
        .await
        .expect("补迁应该成功");
        assert_eq!(outcome.migration.action, "backfill");
        assert_eq!(
            outcome.migration.migration_state,
            WorkspaceMigrationState::Migrated,
            "补迁不翻标记"
        );
        assert_eq!(
            outcome.migration.steps,
            [
                "in-flight writes drained",
                "agent stopped",
                "volume detached",
                "2 path(s) backfilled, 1 skipped (missing on the shared volume)",
                "agent restarted on RBD",
            ]
        );
        assert_eq!(
            harness.probe.calls(),
            [
                "stop",
                "backfill:shared-pvc:[\"*\", \"proj-a\"]/[\"node_modules/**\"]",
                "ensure",
            ],
            "顺序不能变: 先停、再补、最后才拉起 agent"
        );
        assert!(
            harness.probe.agent_present.load(Ordering::SeqCst),
            "agent 要回来"
        );
    }

    /// 闲置用户（agent 已被回收）：只动数据, 不为它拉起 agent——与 cutover、rollback 同一口径。
    #[tokio::test]
    async fn an_idle_agent_is_left_stopped() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            ..Default::default()
        })
        .await;
        let outcome = backfill(&harness.state, USER, "shared", &request(&[], &[]))
            .await
            .expect("补迁应该成功");
        // 停 agent 照样调一次（对不在的 agent 是空操作）, 与 cutover 同一条路径
        assert_eq!(
            outcome.migration.steps,
            [
                "in-flight writes drained",
                "agent stopped",
                "volume detached",
                "0 path(s) backfilled, 1 skipped (missing on the shared volume)",
                "agent left stopped (not running before backfill)"
            ]
        );
        assert_eq!(
            harness.probe.calls(),
            ["stop", "backfill:shared:[]/[]"],
            "不拉起 agent"
        );
    }

    /// cutover 最严重的事故形态同样适用于补迁：Job 失败之后 agent 不能永久停在 0 副本。
    #[tokio::test]
    async fn a_failed_job_still_brings_the_agent_back() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            agent_running: true,
            backfill_fails: true,
        })
        .await;
        let error = backfill(&harness.state, USER, "shared", &request(&[".local"], &[]))
            .await
            .expect_err("Job 失败时补迁必须失败");
        let message = error.to_string();
        assert!(
            message.contains("migration step 'backfill' failed")
                && message.contains("out of space"),
            "失败原因要带出来: {message}"
        );
        assert_eq!(
            harness.probe.calls(),
            ["stop", "backfill:shared:[\".local\"]/[]", "ensure"],
            "Job 失败之后也要把 agent 拉回来"
        );
        assert!(harness.probe.agent_present.load(Ordering::SeqCst));
        assert!(
            message.contains("; agent restarted on RBD"),
            "错误里要说明 agent 已经拉回来了: {message}"
        );
    }

    /// Job 失败且用户本来就闲置：没有 agent 要拉, 也不该为一个闲置用户拉起一个。
    #[tokio::test]
    async fn a_failed_job_for_an_idle_user_does_not_wake_the_agent() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            backfill_fails: true,
            ..Default::default()
        })
        .await;
        assert!(
            backfill(&harness.state, USER, "shared", &request(&[], &[]))
                .await
                .is_err()
        );
        assert_eq!(harness.probe.calls(), ["stop", "backfill:shared:[]/[]"]);
        assert!(!harness.probe.agent_present.load(Ordering::SeqCst));
    }

    /// 编排锁与写门禁在成败两条路径上都要放开：第二次补迁不能被第一次留下的锁卡住。
    #[tokio::test]
    async fn the_lock_and_the_write_gate_do_not_leak_past_a_run() {
        let harness = harness_of(&Scenario {
            migrated: true,
            pvc_exists: true,
            agent_running: true,
            ..Default::default()
        })
        .await;
        backfill(&harness.state, USER, "shared", &request(&[], &[]))
            .await
            .expect("第一次应该成功");
        harness.probe.backfill_fails.store(true, Ordering::SeqCst);
        assert!(
            backfill(&harness.state, USER, "shared", &request(&[], &[]))
                .await
                .is_err(),
            "第二次按设定失败"
        );
        assert_eq!(
            harness.probe.calls(),
            [
                "stop",
                "backfill:shared:[]/[]",
                "ensure",
                "stop",
                "backfill:shared:[]/[]",
                "ensure"
            ],
            "成败各一次, 第二次没有被第一次留下的锁或门禁挡住"
        );
        assert!(
            !shared_types::migration_gate::is_migrating(USER),
            "写门禁必须已经解除"
        );
    }
}
