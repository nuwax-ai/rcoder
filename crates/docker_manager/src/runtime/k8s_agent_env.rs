//! agent 容器 env 组装（从 k8s_agent_create.rs 的 build_agent_pod_spec 拆出）。
//!
//! 纯函数无 K8s API 副作用: 基础标识（PROJECT_ID/USER_ID/SERVICE_TYPE/DEPLOY_MODE）、
//! 多租户、service environment 合并（RESERVED 去重, docker 兜底→k8s 覆盖）、
//! UserappBuilder 挂载压平契约 env 注入（摘除 config 同名防覆盖）、release lock
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
    // 容器内契约（agent_runner profiler 标签、PROJECT_ID env）要纯 app_id：
    // 优先取显式 builder_app_id（创建路径直传，语义不重载）；存量复合键
    // 残留时右切还原纯 app 段，其余原样透传。
    let project_id_for_env = match params.builder_app_id.as_deref() {
        Some(app_id) => app_id.to_string(),
        None if matches!(service_type, ServiceType::UserappBuilder) => {
            shared_types::legacy_composite_app_segment(project_id_val).to_string()
        }
        None => project_id_val.to_string(),
    };
    let mut env_vars = vec![
        EnvVar {
            name: "PROJECT_ID".to_string(),
            value: Some(project_id_for_env.clone()),
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
    // R07：UserappBuilder 容器注入稳定状态根（app-cli 运行内核的操作记录/
    // desired 持久化锚点，env 权威——dev 容器内 source/.run 轮换不漂移锁域；
    // 路径=workspace 卷内专属 state 子目录，跨容器重建稳定）
    if matches!(service_type, ServiceType::UserappBuilder) {
        env_vars.push(EnvVar {
            name: "APP_CLI_STATE_ROOT".to_string(),
            value: Some(format!(
                "{}/{}/state/{}",
                shared_types::paths::USERAPP_DEV_HOME,
                project_id_for_env,
                project_id_for_env,
            )),
            ..Default::default()
        });
    }
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
    // UserappBuilder 挂载压平契约 env 是平台注入的固定值（与四
    // subPath 挂载点绑定）——先从 merged_env 摘除, 防 config
    // environment 覆盖造成数据面分裂（PGDATA 落 overlay = builder
    // 重建丢库）。
    if matches!(service_type, ServiceType::UserappBuilder) {
        for var in [
            "USERAPP_WORKSPACE_DIR",
            "USERAPP_LOG_DIR",
            "PGDATA",
            "DBX_DATA_DIR",
        ] {
            merged_env.remove(var);
        }
    }
    // B03：managed owner 注入链补全。灰度开关=service env 显式
    // APP_CLI_MANAGED=1（ops 级部署决策，非按请求）——开启时自动补：
    // - APP_CLI_RUNTIME_WORKSPACE：真实 workspace（解包根 code/），
    //   取代包装脚本缺省 empty（不再出现 empty owner 抢 3010）；
    // - APP_CLI_DEPLOY_TOKEN：按创建生成的部署凭据——owner 落盘状态根
    //   （共享卷），file-server 经 R09 同一解析契约读取提交运行操作；
    //   token 不入日志/事件/描述。
    // 关闭（缺省）不注入：包装脚本 no-op，legacy 行为不变。
    if matches!(service_type, ServiceType::UserappBuilder)
        && merged_env
            .get("APP_CLI_MANAGED")
            .is_some_and(|value| value == "1")
    {
        env_vars.push(EnvVar {
            name: "APP_CLI_RUNTIME_WORKSPACE".to_string(),
            value: Some(shared_types::paths::app_code_root(&project_id_for_env)),
            ..Default::default()
        });
        env_vars.push(EnvVar {
            name: "APP_CLI_DEPLOY_TOKEN".to_string(),
            value: Some(uuid::Uuid::new_v4().simple().to_string()),
            ..Default::default()
        });
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
    // UserappBuilder 挂载压平契约 env（与上方四 subPath 挂载点绑定,
    // 值为 shared_types::paths 单一事实源）。PGDATA/DBX_DATA_DIR
    // 使 dev PG/dbx 数据落卷持久（镜像脚本均为 ${VAR:-...} 覆盖模式,
    // 无 env 时落 overlay, builder 重建即丢）。
    if matches!(service_type, ServiceType::UserappBuilder) {
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
    // 透传 Userapp build 必需 env 给 agent-runner（build 在 agent-runner 执行）:
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

#[cfg(all(test, feature = "kubernetes"))]
mod b03_tests {
    use super::*;
    use container_runtime_api::ContainerCreateParams;

    fn params() -> ContainerCreateParams {
        ContainerCreateParams::builder()
            .builder_app_id("app-b03".to_string())
            .build()
    }

    fn k8s_service(managed: bool) -> K8sServiceConfig {
        let mut config = K8sServiceConfig {
            service_type: ServiceType::UserappBuilder,
            image: None,
            arm64_image: None,
            amd64_image: None,
            default_image: None,
            image_tag_prefix: None,
            enabled: true,
            environment: std::collections::HashMap::new(),
            command: vec![],
            workspace_resolution_path: None,
            resource_limits: shared_types::ServiceResourceLimits::default(),
            volumes: vec![],
            volume_mounts: vec![],
            sidecars: vec![],
        };
        if managed {
            config
                .environment
                .insert("APP_CLI_MANAGED".to_string(), "1".to_string());
        }
        config
    }

    fn value_of(vars: &[EnvVar], name: &str) -> Option<String> {
        vars.iter()
            .find(|var| var.name == name)
            .and_then(|var| var.value.clone())
    }

    /// B03：service env 显式 APP_CLI_MANAGED=1 → 注入链补全（真实
    /// workspace + 部署凭据），关闭（缺省）不注入。
    #[test]
    fn managed_switch_completes_owner_injection_chain() {
        let base = params();
        // 关闭（缺省）：无 workspace/token 注入（包装脚本 no-op）
        let off = build_agent_env_vars(
            "app-b03",
            "u1",
            "userapp-builder",
            &ServiceType::UserappBuilder,
            None,
            Some(&k8s_service(false)),
            &base,
        );
        assert!(value_of(&off, "APP_CLI_RUNTIME_WORKSPACE").is_none());
        assert!(value_of(&off, "APP_CLI_DEPLOY_TOKEN").is_none());
        assert_eq!(
            value_of(&off, "APP_CLI_STATE_ROOT").as_deref(),
            Some("/home/user/app-b03/state/app-b03"),
            "state root注入不受开关影响（R07 既有行为）"
        );

        // 开启：workspace=解包根 code/，token 生成非空
        let on = build_agent_env_vars(
            "app-b03",
            "u1",
            "userapp-builder",
            &ServiceType::UserappBuilder,
            None,
            Some(&k8s_service(true)),
            &base,
        );
        assert_eq!(
            value_of(&on, "APP_CLI_RUNTIME_WORKSPACE").as_deref(),
            Some("/home/user/app-b03/code"),
            "managed 开启必须注入真实 workspace（不是包装脚本缺省 empty）"
        );
        let token = value_of(&on, "APP_CLI_DEPLOY_TOKEN").expect("token injected");
        assert!(
            !token.trim().is_empty() && token.len() >= 32,
            "token={token}"
        );
    }
}
