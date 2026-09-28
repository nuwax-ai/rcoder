use super::*;

#[test]
fn builder_reuse_requires_lifecycle_configuration_and_template_identity() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app-one".into(),
        lifecycle_id: "life-one".into(),
        operation_id: "operation-one".into(),
        executor_id: "executor-one".into(),
        request_fingerprint: "ab".repeat(32),
    };
    let mut annotations = context.resource_metadata();
    annotations.insert(TEMPLATE_HASH_ANNOTATION.into(), "template-one".into());
    let desired = StatefulSet {
        metadata: ObjectMeta {
            annotations: Some(annotations.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    annotations: Some(annotations),
                    ..Default::default()
                }),
                spec: Some(sample_pod_spec("builder:one")),
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_builder_statefulset(&desired, &desired, &context).is_ok());
    let mut next_operation = context.clone();
    next_operation.operation_id = "operation-two".into();
    assert!(validate_builder_statefulset(&desired, &desired, &next_operation).is_ok());
    next_operation.lifecycle_id = "life-two".into();
    assert!(validate_builder_statefulset(&desired, &desired, &next_operation).is_err());
    let mut foreign = desired.clone();
    foreign.metadata.annotations = None;
    assert!(validate_builder_statefulset(&foreign, &desired, &context).is_err());
    foreign = desired.clone();
    foreign.spec.as_mut().unwrap().template.metadata = None;
    assert!(validate_builder_statefulset(&foreign, &desired, &context).is_err());
    foreign = desired.clone();
    foreign
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(TEMPLATE_HASH_ANNOTATION.into(), "other-template".into());
    // 注解与期望指纹不等但 launch 内容完全一致 = 旧算法存量注解：
    // 授权自愈（StaleHash），不再当作漂移围栏（方案 c 迁移语义）。
    assert!(matches!(
        validate_builder_statefulset(&foreign, &desired, &context),
        Ok(BuilderTemplateCheck::StaleHash)
    ));
}

fn builder_sts_with_env(
    hash: &str,
    env: Vec<EnvVar>,
    context: &shared_types::UserAppExecutionContext,
) -> StatefulSet {
    use k8s_openapi::api::core::v1::Container;
    let mut annotations = context.resource_metadata();
    annotations.insert(TEMPLATE_HASH_ANNOTATION.into(), hash.into());
    let pod_spec = PodSpec {
        containers: vec![Container {
            name: "agent".to_string(),
            image: Some("builder:one".to_string()),
            env: Some(env),
            ..Default::default()
        }],
        ..Default::default()
    };
    StatefulSet {
        metadata: ObjectMeta {
            annotations: Some(annotations.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    annotations: Some(annotations),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// 方案 c 自愈（迁移语义）：存量 STS 的 template-hash 是旧算法写入的值，
/// 与新算法期望指纹必然不等——但 launch 内容（含构造顺序不同的 env，
/// 归一化后等价）完全一致时必须授权自愈而非误报漂移。修复前必红：
/// 跨算法注解 mismatch 一律 Conflict，存量 STS 首次全量 ensure 即落
/// 围栏/收束循环（app 151/155 事故修复的迁移缺口）。
#[test]
fn stale_hash_annotation_heals_when_launch_content_equivalent() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app-one".into(),
        lifecycle_id: "life-one".into(),
        operation_id: "operation-one".into(),
        executor_id: "executor-one".into(),
        request_fingerprint: "ab".repeat(32),
    };
    let order_a = vec![
        EnvVar {
            name: "ALPHA".into(),
            value: Some("1".into()),
            ..Default::default()
        },
        EnvVar {
            name: "BETA".into(),
            value: Some("2".into()),
            ..Default::default()
        },
    ];
    let order_b: Vec<EnvVar> = order_a.iter().rev().cloned().collect();
    let desired = builder_sts_with_env("hash-current", order_a, &context);
    let mut existing = builder_sts_with_env("hash-legacy", order_b, &context);
    assert!(matches!(
        validate_builder_statefulset(&existing, &desired, &context),
        Ok(BuilderTemplateCheck::StaleHash)
    ));
    // 迁移完成后（注解已被 heal 为当前算法值）→ Current。
    existing
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(TEMPLATE_HASH_ANNOTATION.into(), "hash-current".into());
    assert!(matches!(
        validate_builder_statefulset(&existing, &desired, &context),
        Ok(BuilderTemplateCheck::Current)
    ));
}

/// 自愈不放松真实漂移：非挥发性 env 值变化（注解也同时 mismatch）必须
/// 仍然拒绝——内容等价是自愈的唯一授权，注解门自身不构成漂移证据。
#[test]
fn stale_hash_heal_never_masks_environment_drift() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app-one".into(),
        lifecycle_id: "life-one".into(),
        operation_id: "operation-one".into(),
        executor_id: "executor-one".into(),
        request_fingerprint: "ab".repeat(32),
    };
    let env = vec![EnvVar {
        name: "ALPHA".into(),
        value: Some("1".into()),
        ..Default::default()
    }];
    let drifted_env = vec![EnvVar {
        name: "ALPHA".into(),
        value: Some("2".into()),
        ..Default::default()
    }];
    let desired = builder_sts_with_env("hash-current", env, &context);
    let existing = builder_sts_with_env("hash-legacy", drifted_env, &context);
    assert!(validate_builder_statefulset(&existing, &desired, &context).is_err());
}

/// 事故回归闸（2026-09-22 app-155 重启恒失败，"drifted keys: ~USER_ID"）：
/// apiserver 对 env value="" 存储不可往返——desired 发 `Some("")` 的条目
/// （builder 的 USER_ID 恒空），API 读回是无 value 字段（`None`）。二者
/// 必须判等，否则全部存量 builder STS 的 ensure/restart 恒 Conflict。
#[test]
fn empty_string_env_roundtrip_is_equivalent() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app-one".into(),
        lifecycle_id: "life-one".into(),
        operation_id: "operation-one".into(),
        executor_id: "executor-one".into(),
        request_fingerprint: "ab".repeat(32),
    };
    // desired：创建链构造形态（USER_ID = Some("")，agent/builder 共用模板）
    let desired_env = vec![
        EnvVar {
            name: "USER_ID".into(),
            value: Some(String::new()),
            ..Default::default()
        },
        EnvVar {
            name: "PROJECT_ID".into(),
            value: Some("155".into()),
            ..Default::default()
        },
    ];
    // existing：apiserver 存回形态（USER_ID 无 value 字段）
    let stored_env = vec![
        EnvVar {
            name: "USER_ID".into(),
            value: None,
            ..Default::default()
        },
        EnvVar {
            name: "PROJECT_ID".into(),
            value: Some("155".into()),
            ..Default::default()
        },
    ];
    let desired = builder_sts_with_env("hash-current", desired_env, &context);
    let existing = builder_sts_with_env("hash-current", stored_env.clone(), &context);
    assert!(matches!(
        validate_builder_statefulset(&existing, &desired, &context),
        Ok(BuilderTemplateCheck::Current)
    ));
    // 反向同样成立（两侧归一对称）；真实值漂移仍被拒（防归一变放宽）。
    assert!(matches!(
        validate_builder_statefulset(&desired, &existing, &context),
        Ok(BuilderTemplateCheck::Current)
    ));
    let mut drifted = stored_env.clone();
    drifted[0].value = Some("real-user".into());
    let drifted_sts = builder_sts_with_env("hash-current", drifted, &context);
    assert!(validate_builder_statefulset(&drifted_sts, &desired, &context).is_err());
}

#[test]
fn scale_patch_keeps_captured_identity_and_rejects_unowned_resources() {
    let mut existing = StatefulSet {
        metadata: ObjectMeta {
            name: Some("builder-one".into()),
            uid: Some("physical-original".into()),
            resource_version: Some("17".into()),
            labels: Some(
                [(
                    SERVICE_TYPE_LABEL.into(),
                    ServiceType::UserappBuilder.to_string(),
                )]
                .into(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    let patch = conditional_scale_patch(&existing, &ServiceType::UserappBuilder, 1).unwrap();
    assert_eq!(patch["metadata"]["uid"], "physical-original");
    assert_eq!(patch["metadata"]["resourceVersion"], "17");
    assert_eq!(patch["spec"]["replicas"], 1);
    assert!(conditional_scale_patch(&existing, &ServiceType::Userapp, 1).is_err());
    existing.metadata.resource_version = None;
    assert!(conditional_scale_patch(&existing, &ServiceType::UserappBuilder, 1).is_err());
}

fn sample_pod_spec(image: &str) -> PodSpec {
    use k8s_openapi::api::core::v1::{Container, EnvVar};
    PodSpec {
        containers: vec![Container {
            name: "agent".to_string(),
            image: Some(image.to_string()),
            env: Some(vec![EnvVar {
                name: "AGENT_MODE".to_string(),
                value: Some("standard".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// 确定性：同参数两次构造（独立对象）指纹相等——跨副本/重启稳定是
/// 漂移检测不误报的前提。
#[test]
fn template_hash_is_deterministic_for_same_input() {
    let a = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
    let b = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
    assert_eq!(a, b);
    assert!(!a.is_empty());
}

/// 敏感性：镜像变更必须反映到指纹（升版检测的主场景）。
#[test]
fn template_hash_changes_when_image_changes() {
    let old = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
    let new = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.231"));
    assert_ne!(old, new);
}

/// 敏感性：非镜像字段（env）变更也必须反映（config 变更场景）。
#[test]
fn template_hash_changes_when_env_changes() {
    let mut spec = sample_pod_spec("repo/rcoder:0.1.230");
    let before = agent_template_hash(&spec);
    if let Some(env) = spec.containers[0].env.as_mut() {
        env[0].value = Some("advanced".to_string());
    }
    let after = agent_template_hash(&spec);
    assert_ne!(before, after);
}

/// 确定性（跨构造顺序）：env 列表由 HashMap 迭代组装，两个副本对同一
/// 期望配置可能以不同顺序构造 spec——指纹必须与 env 顺序无关。
/// 修复前必红：HashMap 每进程随机迭代序使复用校验对完全匹配的
/// 控制器误报 "configuration changed"（测试环境 app 151/155 事故根因）。
#[test]
fn template_hash_is_independent_of_env_ordering() {
    use k8s_openapi::api::core::v1::EnvVar;
    let entries = [
        ("ALPHA", "1"),
        ("BETA", "2"),
        ("GAMMA", "3"),
        ("DELTA", "4"),
        ("EPSILON", "5"),
    ];
    let build = |order: usize| {
        let mut spec = sample_pod_spec("repo/rcoder:0.1.230");
        let rotated: Vec<EnvVar> = (0..entries.len())
            .map(|index| {
                let (name, value) = entries[(index + order) % entries.len()];
                EnvVar {
                    name: name.to_string(),
                    value: Some(value.to_string()),
                    ..Default::default()
                }
            })
            .collect();
        spec.containers[0].env = Some(rotated);
        agent_template_hash(&spec)
    };
    let reference = build(0);
    for order in 1..entries.len() {
        assert_eq!(
            reference,
            build(order),
            "env ordering must not affect the template hash"
        );
    }
}

/// 确定性（凭据排除）：APP_CLI_DEPLOY_TOKEN 每次生成都换新随机值，
/// 不得影响指纹——修复前两次 ensure 的指纹必然不同，复用校验形同虚设。
#[test]
fn template_hash_ignores_per_creation_deploy_token() {
    use k8s_openapi::api::core::v1::EnvVar;
    let build = |token: &str| {
        let mut spec = sample_pod_spec("repo/rcoder:0.1.230");
        spec.containers[0]
            .env
            .get_or_insert_with(Vec::new)
            .push(EnvVar {
                name: "APP_CLI_DEPLOY_TOKEN".to_string(),
                value: Some(token.to_string()),
                ..Default::default()
            });
        agent_template_hash(&spec)
    };
    assert_eq!(build("token-aaa"), build("token-bbb"));
}

/// per-request 字段豁免：resources 与 TENANT_ID/SPACE_ID/ISOLATION_TYPE 随
/// 请求抖动，不得进入指纹（否则同版本 ensure 对比误报 drift，参数噪声
/// 淹没版本信号）。
#[test]
fn template_hash_ignores_per_request_fields() {
    let base = sample_pod_spec("repo/rcoder:0.1.230");
    // 调资源限额
    let mut with_resources = base.clone();
    with_resources.containers[0].resources =
        Some(k8s_openapi::api::core::v1::ResourceRequirements {
            limits: Some([("cpu".to_string(), quantity("2"))].into_iter().collect()),
            ..Default::default()
        });
    assert_eq!(
        agent_template_hash(&base),
        agent_template_hash(&with_resources),
        "resources change must not affect template hash"
    );
    // 注入隔离 env
    let mut with_isolation = base.clone();
    if let Some(env) = with_isolation.containers[0].env.as_mut() {
        env.push(EnvVar {
            name: "TENANT_ID".to_string(),
            value: Some("t1".to_string()),
            ..Default::default()
        });
    }
    assert_eq!(
        agent_template_hash(&base),
        agent_template_hash(&with_isolation),
        "isolation env must not affect template hash"
    );
}

fn quantity(v: &str) -> k8s_openapi::apimachinery::pkg::api::resource::Quantity {
    k8s_openapi::apimachinery::pkg::api::resource::Quantity(v.to_string())
}
