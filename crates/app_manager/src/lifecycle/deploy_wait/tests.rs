use super::*;
use shared_types::{AppCliDeployPhase as Phase, AppDeploymentStage as Stage};

fn operation_probe(op: &str, stage: Stage, phase: Phase) -> DeployStatusProbe {
    parse_deploy_status(&serde_json::json!({"data": {
        "protocol_version": shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL,
        "phase": phase,
        "release_id": "manifest-build",
        "request_release_id": "request-build",
        "operation": {
            "operation_id": op, "deployment_generation_id": op,
            "request_release_id": "request-build", "artifact_release_id": "manifest-build",
            "deploy_stage": stage, "persisted": true, "phase": phase, "error": "deployment failed"
        }
    }}))
}

#[test]
fn cold_stage_uses_operation_not_request_or_manifest_identity() {
    assert_eq!(
        judge_stage(
            &operation_probe("op-new", Stage::Succeeded, Phase::Orchestrating),
            "op-new"
        ),
        StageVerdict::Done
    );
    assert_eq!(
        judge_stage(
            &operation_probe("op-old", Stage::Succeeded, Phase::Running),
            "op-new"
        ),
        StageVerdict::Pending
    );
    assert_eq!(
        judge_stage(
            &operation_probe("op-old", Stage::Failed, Phase::Failed),
            "op-new"
        ),
        StageVerdict::Pending
    );
}

#[test]
fn healthy_old_service_cannot_hide_failed_deployment() {
    assert!(matches!(
        judge_stage(
            &operation_probe("op-new", Stage::Failed, Phase::Running),
            "op-new"
        ),
        StageVerdict::VerifiedFailed(_)
    ));
    assert_eq!(
        judge_stage(
            &operation_probe("op-new", Stage::Pending, Phase::Running),
            "op-new"
        ),
        StageVerdict::Pending
    );
    // Cold acceptance guarantees artifact activation, not later business readiness.
    assert_eq!(
        judge_stage(
            &operation_probe("op-new", Stage::Succeeded, Phase::Failed),
            "op-new"
        ),
        StageVerdict::Done
    );
}

#[test]
fn contract_error_with_matching_op_id_and_persisted_failed_is_not_verified() {
    // 计划 1c 反例：同 operation_id 但 generation 不匹配的 persisted Failed——
    // 判定必须是 ContractError（调用方不得 mark_completed 释放租约）。
    // 修复前该场景走 Failed 分支且调用方只查 persisted && stage==Failed 即释放。
    let probe = parse_deploy_status(&serde_json::json!({"data": {
        "protocol_version": shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL,
        "phase": Phase::Failed,
        "release_id": "manifest-build",
        "request_release_id": "request-build",
        "operation": {
            "operation_id": "op-new",
            "deployment_generation_id": "a-different-generation",
            "request_release_id": "request-build",
            "artifact_release_id": "manifest-build",
            "deploy_stage": Stage::Failed,
            "persisted": true,
            "phase": Phase::Failed,
            "error": "deployment failed"
        }
    }}));
    assert!(matches!(
        judge_stage(&probe, "op-new"),
        StageVerdict::ContractError(_)
    ));
    // 协议版本不匹配（旧镜像 app-cli）同理：结果未知，不释放
    let stale_protocol = parse_deploy_status(&serde_json::json!({"data": {
        "protocol_version": 3u32,
        "phase": Phase::Failed,
        "operation": {
            "operation_id": "op-new",
            "deployment_generation_id": "op-new",
            "request_release_id": "request-build",
            "artifact_release_id": "manifest-build",
            "deploy_stage": Stage::Failed,
            "persisted": true,
            "phase": Phase::Failed,
            "error": "deployment failed"
        }
    }}));
    assert!(matches!(
        judge_stage(&stale_protocol, "op-new"),
        StageVerdict::ContractError(_)
    ));
}

#[test]
fn missing_operation_never_falls_back_to_release_id() {
    let probe = parse_deploy_status(
        &serde_json::json!({"phase":"running", "release_id":"op-new", "request_release_id":"op-new"}),
    );
    assert!(!matches!(judge_stage(&probe, "op-new"), StageVerdict::Done));
}

#[test]
fn declaration_watchdog_fires_only_after_sustained_absence() {
    let start = tokio::time::Instant::now();
    let mut watchdog = DeclarationWatchdog::default();
    assert!(
        !watchdog.observe(false, start),
        "grace period must not fire immediately"
    );
    assert!(!watchdog.observe(
        false,
        start + RUNTIME_DECLARATION_GRACE - Duration::from_secs(1)
    ));
    assert!(watchdog.observe(false, start + RUNTIME_DECLARATION_GRACE));
    // The expected operation appearing resets the window; a fresh absence
    // restarts the clock from its first observation.
    let resumed = start + RUNTIME_DECLARATION_GRACE + Duration::from_secs(1);
    assert!(!watchdog.observe(true, resumed));
    let absent_at = resumed + RUNTIME_DECLARATION_GRACE - Duration::from_secs(1);
    assert!(!watchdog.observe(false, absent_at));
    assert!(!watchdog.observe(
        false,
        absent_at + RUNTIME_DECLARATION_GRACE - Duration::from_secs(1)
    ));
    assert!(watchdog.observe(false, absent_at + RUNTIME_DECLARATION_GRACE));
}

#[test]
fn declaration_watchdog_counts_mismatched_operation_as_absent() {
    // 与 judge_stage 的 op_id 不匹配语义一致：旧操作的终态不算"本次声明已接受"。
    let probe = operation_probe("op-old", Stage::Succeeded, Phase::Running);
    let present = probe
        .operation
        .as_ref()
        .is_some_and(|operation| operation.operation_id == "op-new");
    assert!(!present);
}

#[test]
fn both_wire_shapes_preserve_operation_contract() {
    let probe = operation_probe("op", Stage::Pending, Phase::Deploying);
    let op = probe.operation.unwrap();
    let bare = serde_json::json!({"protocol_version": 4, "operation": op, "phase": "deploying"});
    assert_eq!(
        parse_deploy_status(&bare).operation.unwrap().operation_id,
        "op"
    );
    assert!(
        parse_deploy_status(&serde_json::json!({"data":null}))
            .operation
            .is_none()
    );
}
