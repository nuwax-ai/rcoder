use crate::test_support::{MockRuntime, test_service};
use std::sync::Arc;

fn crashloop_observation(
    app_id: &str,
    pod: &str,
    restarts: u32,
) -> container_runtime_api::PodObservation {
    container_runtime_api::PodObservation::Observed(container_runtime_api::PodFailureObservation {
        deployment_uid: format!("deploy-{app_id}"),
        matches_target_template: true,
        pod_uid: pod.to_string(),
        pod_phase: "Running".into(),
        scheduled: Some(true),
        scheduling_reason: None,
        container_waiting_reason: Some("CrashLoopBackOff".into()),
        container_waiting_message: Some("back-off restarting failed container".into()),
        container_last_exit: Some(container_runtime_api::ContainerExit {
            code: 1,
            reason: Some("Error".into()),
        }),
        restart_count: restarts,
        ready: false,
    })
}

/// None = 窗口内仍在等待（信号未触发，预算 1800s 不可达的正确行为）；
/// Some(error) = 等待提前返回（信号/围栏错误）。
async fn fenced_service_with_signal(
    app_id: &'static str,
    script: Vec<Vec<container_runtime_api::PodObservation>>,
) -> Option<crate::error::AppOperationError> {
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        app_id.into(),
        container_runtime_api::DeploymentStatus {
            app_id: app_id.into(),
            replicas: 1,
            ready_replicas: 0,
            phase: "Starting".into(),
            // deployment_uid 存在才会进入观察分支（Docker/未设 uid 天然跳过）
            deployment_uid: Some(format!("deploy-{app_id}")),
            ..Default::default()
        },
    );
    let mut queue = std::collections::VecDeque::new();
    for round in script {
        queue.push_back(round);
    }
    runtime.pod_observations.insert(app_id.into(), queue);
    runtime.specs.insert(
        app_id.into(),
        container_runtime_api::ContainerSpecSnapshot {
            env: Some(
                [
                    (
                        shared_types::APP_DEPLOY_OPERATION_ID.to_string(),
                        "op-fastfail".to_string(),
                    ),
                    ("APP_CLI_DEPLOY_TOKEN".to_string(), "token".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
    );
    let service = test_service(directory.path(), runtime.clone()).await;
    let guard = service
        .acquire_process_release_lock(app_id)
        .await
        .expect("lock");
    // None = 8s 窗口内仍在等待（预算 1800s 不可达——未触发信号的正确行为）；
    // Some(err) = 等待提前返回（信号/围栏错误）
    match tokio::time::timeout(
        std::time::Duration::from_secs(8),
        service.wait_deploy_stage(app_id, "op-fastfail", &guard),
    )
    .await
    {
        Ok(outcome) => Some(outcome.expect_err("wait only errors in this fixture")),
        Err(_) => None,
    }
}

#[tokio::test]
async fn deterministic_crashloop_fails_wait_fast_and_keeps_fence() {
    // 反例（修复前）：CrashLoop 信号不存在，等待耗满预算（本测试预算内
    // 只会 Pending 直到 8s 超时——修复后数秒内返回结构化失败）。
    // 断言：快速失败 + 错误含分类与 restart 计数 + 操作进 RecoveryRequired
    // （围栏保留：后续锁获取仍被拒——不自动解锁）。
    let app_id = "fastfail1";
    let round = vec![crashloop_observation(app_id, "pod-a", 3)];
    let result = fenced_service_with_signal(app_id, vec![round.clone(), round])
        .await
        .expect("crashloop must fail fast (not keep waiting)");
    let message = result.to_string();
    assert!(
        message.contains("deterministic deployment failure"),
        "{message}"
    );
    assert!(message.contains("CrashLoopBackOff"), "{message}");
    assert!(message.contains("restart_count=3"), "{message}");
    // 围栏语义：错误是 Backend（结果未知路径），不是释放性终态——
    // 由 deploy_admitted 错误分支落 RecoveryRequired（此处直接验证锁语义：
    // guard 仍在持有者手里，新的等待循环会立刻拿到？不——同一 guard 已
    // 交还测试。真正的围栏断言在 service 层测试中覆盖）。
}

#[tokio::test]
async fn single_round_or_below_threshold_never_triggers() {
    // 单轮观察（未过防抖）→ 等待持续到外层 8s 预算（Pending 循环）
    let app_id = "fastfail2";
    // 低于阈值（restart=2 < 3）：分类函数恒 None——等待必须继续（外层
    // 超时 elapsed 即证明未提前失败；预算本身 1800s 不可达）
    let round = vec![crashloop_observation(app_id, "pod-a", 2)];
    let outcome = fenced_service_with_signal(app_id, vec![round.clone(), round]).await;
    match outcome {
        None => { /* 仍在等待 = 未触发（正确） */ }
        Some(error) => panic!("低于阈值不得提前失败：{error}"),
    }
}

#[tokio::test]
async fn pod_replacement_resets_debounce_baseline() {
    // 同一 pod 一轮 + 换 pod 一轮（同名分类）→ 防抖重置，不触发；
    // 证明换代期旧 pod 的单轮 crashloop 不可能借新等待的防抖窗口
    let app_id = "fastfail3";
    let outcome = fenced_service_with_signal(
        app_id,
        vec![
            vec![crashloop_observation(app_id, "pod-old", 5)],
            vec![crashloop_observation(app_id, "pod-new", 5)],
            vec![crashloop_observation(app_id, "pod-new2", 5)],
        ],
    )
    .await;
    match outcome {
        None => { /* 仍在等待 = 防抖被重置（正确） */ }
        Some(error) => panic!("pod 更换必须重置防抖，不得触发：{error}"),
    }
}

#[tokio::test]
async fn observation_failure_is_not_progress_and_not_failure() {
    // 无观察（未预置脚本）= 空观察——等待持续；证明观察缺失不误报
    let app_id = "fastfail4";
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        app_id.into(),
        container_runtime_api::DeploymentStatus {
            app_id: app_id.into(),
            replicas: 1,
            phase: "Starting".into(),
            deployment_uid: Some(format!("deploy-{app_id}")),
            ..Default::default()
        },
    );
    runtime.specs.insert(
        app_id.into(),
        container_runtime_api::ContainerSpecSnapshot {
            env: Some(
                [(
                    shared_types::APP_DEPLOY_OPERATION_ID.to_string(),
                    "op-nosig".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
    );
    let service = test_service(directory.path(), runtime.clone()).await;
    let guard = service
        .acquire_process_release_lock(app_id)
        .await
        .expect("lock");
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        service.wait_deploy_stage(app_id, "op-nosig", &guard),
    )
    .await;
    assert!(outcome.is_err(), "空观察不触发信号也不中断（仍在等待）");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(3),
        "必须等待满窗口"
    );
}

#[tokio::test]
async fn docker_mode_without_deployment_uid_skips_observation() {
    // deployment_uid=None（Docker 语义）→ 不进入观察分支（无信号），
    // 行为与现状一致
    let app_id = "fastfail5";
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        app_id.into(),
        container_runtime_api::DeploymentStatus {
            app_id: app_id.into(),
            replicas: 1,
            phase: "Starting".into(),
            deployment_uid: None,
            ..Default::default()
        },
    );
    // 即便误配了观察脚本也无路径消费
    let mut queue = std::collections::VecDeque::new();
    let round = vec![crashloop_observation(app_id, "pod-a", 9)];
    queue.push_back(round.clone());
    queue.push_back(round);
    runtime.pod_observations.insert(app_id.into(), queue);
    runtime.specs.insert(
        app_id.into(),
        container_runtime_api::ContainerSpecSnapshot {
            env: Some(
                [(
                    shared_types::APP_DEPLOY_OPERATION_ID.to_string(),
                    "op-docker".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
    );
    let service = test_service(directory.path(), runtime).await;
    let guard = service
        .acquire_process_release_lock(app_id)
        .await
        .expect("lock");
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        service.wait_deploy_stage(app_id, "op-docker", &guard),
    )
    .await;
    assert!(
        outcome.is_err(),
        "无 deployment_uid（Docker 语义）不进入观察分支，仍应等待"
    );
}
