//! agent 容器 env 组装（从 k8s_agent_create.rs 的 build_agent_pod_spec 拆出）。
//!
//! 纯函数无 K8s API 副作用: 基础标识（PROJECT_ID/USER_ID/SERVICE_TYPE/DEPLOY_MODE）、
//! 多租户、service environment 合并（RESERVED 去重, docker 兜底→k8s 覆盖）、
//! UserAppBuilder 挂载压平契约 env 注入（摘除 config 同名防覆盖）、release lock
//! 三元组透传、build timeout。

#[cfg(feature = "kubernetes")]
use container_runtime_api::ContainerCreateParams;
use k8s_openapi::api::core::v1::EnvVar;
use shared_types::{K8sServiceConfig, ServiceImageConfig, ServiceType};

#[cfg(feature = "kubernetes")]
pub(crate) fn build_agent_env_vars(
    project_id_val: &str,
    user_id_val: &str,
    service_type_str: &str,
    service_type: &ServiceType,
    docker_service: Option<&ServiceImageConfig>,
    k8s_service: Option<&K8sServiceConfig>,
    params: &ContainerCreateParams,
) -> Vec<EnvVar> {
    let mut env_vars = vec![
        EnvVar {
            name: "PROJECT_ID".to_string(),
            value: Some(project_id_val.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "USER_ID".to_string(),
            value: Some(user_id_val.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "SERVICE_TYPE".to_string(),
            value: Some(service_type_str.to_string()),
            ..Default::default()
        },
        // 部署模式标识: start-up.sh 据此 source extra (K8s 下 workspace 是 PVC, 跳过 bind mount 权限修复)
        EnvVar {
            name: "DEPLOY_MODE".to_string(),
            value: Some("k8s".to_string()),
            ..Default::default()
        },
    ];
    // 多租户环境变量（agent_runner 用于构建工作目录路径）
    if let Some(tid) = &params.tenant_id {
        env_vars.push(EnvVar {
            name: "TENANT_ID".to_string(),
            value: Some(tid.clone()),
            ..Default::default()
        });
    }
    if let Some(sid) = &params.space_id {
        env_vars.push(EnvVar {
            name: "SPACE_ID".to_string(),
            value: Some(sid.clone()),
            ..Default::default()
        });
    }
    if let Some(it) = &params.isolation_type {
        env_vars.push(EnvVar {
            name: "ISOLATION_TYPE".to_string(),
            value: Some(it.clone()),
            ..Default::default()
        });
    }
    // 透传 service environment
    // (PROJECT_WORKSPACE_BASE/RUST_LOG/SERVICE_MODE/AGENT_PORT 等,
    //  让 sub-container 行为与 Docker 模式一致)。跳过已硬编码的同名 env。
    // 合并顺序:docker_config 兜底 → kubernetes_config 覆盖(K8s 主)。
    const RESERVED: [&str; 6] = [
        "PROJECT_ID",
        "USER_ID",
        "SERVICE_TYPE",
        "TENANT_ID",
        "SPACE_ID",
        "ISOLATION_TYPE",
    ];
    let mut merged_env: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(sc) = docker_service {
        for (k, v) in &sc.environment {
            merged_env.insert(k.clone(), v.clone());
        }
    }
    if let Some(sc) = k8s_service {
        for (k, v) in &sc.environment {
            merged_env.insert(k.clone(), v.clone());
        }
    }
    // UserAppBuilder 挂载压平契约 env 是平台注入的固定值（与四
    // subPath 挂载点绑定）——先从 merged_env 摘除, 防 config
    // environment 覆盖造成数据面分裂（PGDATA 落 overlay = builder
    // 重建丢库）。
    if matches!(service_type, ServiceType::UserAppBuilder) {
        for var in [
            "USERAPP_WORKSPACE_DIR",
            "USERAPP_LOG_DIR",
            "PGDATA",
            "DBX_DATA_DIR",
        ] {
            merged_env.remove(var);
        }
    }
    for (k, v) in &merged_env {
        if RESERVED.contains(&k.as_str()) {
            continue;
        }
        env_vars.push(EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..Default::default()
        });
    }
    // UserAppBuilder 挂载压平契约 env（与上方四 subPath 挂载点绑定,
    // 值为 shared_types::paths 单一事实源）。PGDATA/DBX_DATA_DIR
    // 使 dev PG/dbx 数据落卷持久（镜像脚本均为 ${VAR:-...} 覆盖模式,
    // 无 env 时落 overlay, builder 重建即丢）。
    if matches!(service_type, ServiceType::UserAppBuilder) {
        for (name, value) in [
            (
                "USERAPP_WORKSPACE_DIR",
                shared_types::paths::USERAPP_DEV_HOME,
            ),
            ("USERAPP_LOG_DIR", shared_types::paths::USERAPP_DEV_LOGS),
            ("PGDATA", shared_types::paths::USERAPP_DEV_PGDATA),
            ("DBX_DATA_DIR", shared_types::paths::USERAPP_DEV_DBX_DATA),
        ] {
            env_vars.push(EnvVar {
                name: name.to_string(),
                value: Some(value.to_string()),
                ..Default::default()
            });
        }
    }
    // 透传 UserApp build 必需 env 给 agent-runner（build 在 agent-runner 执行）:
    // release lock 三元组（rcoder 自身 env 已有，来自 helm runtime identity 注入）。
    // 缺这些 agent-runner 无法生成 release.lock.toml。
    for var in [
        "RCODER_PINGAP_VERSION",
        "RCODER_PINGAP_COMMIT",
        "RCODER_RUNTIME_IMAGE_DIGEST",
    ] {
        if merged_env.contains_key(var) {
            continue;
        }
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            env_vars.push(EnvVar {
                name: var.to_string(),
                value: Some(val),
                ..Default::default()
            });
        }
    }
    // build timeout: rcoder env 透传，缺省 1800s（全量多语言 workspace build）
    if !merged_env.contains_key("DEV_COMMAND_TIMEOUT_SECS") {
        let timeout = std::env::var("DEV_COMMAND_TIMEOUT_SECS")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "1800".to_string());
        env_vars.push(EnvVar {
            name: "DEV_COMMAND_TIMEOUT_SECS".to_string(),
            value: Some(timeout),
            ..Default::default()
        });
    }
    env_vars
}
