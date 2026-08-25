use super::*;

/// 验证 Pod 资源限制配置
///
/// # 参数
/// * `limits` - 资源限制配置
///
/// # 返回
/// Ok(()) 验证通过，Err(String) 返回错误信息
pub(super) fn validate_resource_limits(limits: &ServiceResourceLimits) -> Result<(), String> {
    // 验证 CPU 限制
    if let Some(cpu) = limits.cpu {
        if cpu <= 0.0 {
            return Err("cpu must be greater than 0".to_string());
        }
        if cpu > 128.0 {
            return Err("cpu cannot exceed 128 cores".to_string());
        }
    }

    // 验证内存限制
    if let Some(memory) = limits.memory {
        if memory < 512_000_000.0 {
            return Err("memory must be at least 512MB".to_string());
        }
        if memory > 128_000_000_000.0 {
            return Err("memory cannot exceed 128GB".to_string());
        }
    }

    // 验证 swap 下限。
    // 注:swap 与 memory 的关系不再校验——改为 resolve 阶段 normalize_swap 自动规整
    // (swap < memory 时上调到 memory × 2),避免上游误传 swap<memory 直接阻塞业务。
    if let Some(swap) = limits.swap
        && swap < 512_000_000.0
    {
        return Err("swap must be at least 512MB".to_string());
    }

    // 验证 storage_size 格式（K8s 资源格式）
    if let Some(ref storage_size) = limits.storage_size {
        container_runtime_api::validate_k8s_storage_size(storage_size)?;
    }

    Ok(())
}

/// 容器创建/复用成功后注册 VNC backend 到 pingora 的 `vnc_backends`(显式注册)。
///
/// 背景:pingora `handle_vnc_upstream` 优先走 ContainerLookupService 动态查项目存储
/// (容器存在即可达,与 ttyd 一致、runtime 无关),**回退**到本处填充的 `vnc_backends`
/// 显式注册。显式注册作为兜底数据源——即使项目存储尚未写入或临时查不到,只要注册过
/// 也能解析(双保险)。在 pod/ensure、pod/restart 创建/复用容器后调用本函数主动注册。
///
/// - K8s:用 headless Service FQDN(`{container_name}-svc.{ns}.svc.{cluster_domain}`),
///   走 Service 负载均衡,Pod 重建 IP 变也不影响路由。
/// - Docker:用容器 IP。
/// - 幂等:`add_vnc_backend` 内部 `DashMap::insert` 覆盖语义,restart 重建后自动替换旧地址。
///
/// # 关于 `service_type` 参数(扩展性预留)
/// 当前 VNC 路由按 `user_id` 索引,且**仅 ComputerAgentRunner 使用 VNC**,故本参数现仅用于
/// 日志区分业务场景,不改 key 策略。将来 WebAgentRunner 等其它 service 也支持 VNC 时,需同步:
/// 1. pingora `vnc_backends` 的 key 从纯 `user_id` 改为按 service_type 分桶或复合 key
///    (避免同一 user_id 下不同 service_type 容器互相覆盖);
/// 2. `handle_vnc_upstream`(`rcoder-proxy/src/service/handlers/vnc.rs`)按 service_type 路由;
/// 3. HTTP 路径(当前 `/computer/vnc/{user}/{proj}/websockify`)扩展 service_type 段。
///
/// 届时只需在本函数内决定 key 策略,调用方签名不变(开放-封闭)。
pub(super) fn register_vnc_backend(
    state: &AppState,
    user_id: &str,
    container_info: &ContainerBasicInfo,
    service_type: &ServiceType,
) {
    if let Some(ref pingora) = state.pingora_service {
        // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
        let backend_addr = shared_types::build_backend_addr(
            &container_info.container_name,
            &container_info.container_ip,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        pingora.add_vnc_backend(user_id, &backend_addr);
        debug!(
            "🔗 [POD] VNC backend registered: service_type={:?}, user_id={} -> {}",
            service_type, user_id, backend_addr
        );
    }
}

/// 解析最终生效的资源限制：API 入参优先，缺失字段回退到 configmap 中
/// 该 service_type 的默认配置。
///
/// 背景：Backend 调用容器创建相关接口（`/chat`、`/computer/chat`、`/pod/ensure`、
/// `/pod/restart`）时通常不传 `resource_limits`，直接用 `None` 创建 Pod 会得到
/// 无 requests/limits 的容器（K8s 下 resources 全空）。这里以
/// 默认资源限制的来源按运行时分家（docker/k8s 完全分家 + Fail Fast）:
/// - **K8s 模式**:`kubernetes_config.services[].resource_limits` 是 agent 容器**唯一真源**;
///   service 未配置 → 直接报错(拒绝降级 docker_config,避免"改了 k8s 配置却不生效"的双份真源困惑)。
/// - **Docker 模式**:读 `docker_config.multi_image_config`（K8s 段对 docker 无意义）。
///
/// 无论来源，最终都与 API 显式传入的 `api_limits` 经 `merge_with` 做字段级合并——
/// API 字段优先，未传字段回退默认值。
///
/// 公共核心：直接接受 `ServiceResourceLimits`，所有入口（`/chat`、`/computer/chat`、
/// `/pod/ensure`、`/pod/restart`）统一用 `ServiceResourceLimits`，直接复用本函数。
pub(crate) fn resolve_resource_limits_from_config(
    state: &AppState,
    service_type: &ServiceType,
    api_limits: Option<ServiceResourceLimits>,
) -> Result<Option<ServiceResourceLimits>, AppError> {
    // docker/k8s 完全分家 + Fail Fast:
    // - K8s 模式:kubernetes_config.resource_limits 是 agent 容器唯一真源。service 未配置 =
    //   配置错误,直接报错(拒绝降级 docker_config,避免双份真源困惑)。
    // - Docker 模式:读 docker_config(K8s 段对 docker 无意义)。
    let (default_limits, default_source) = if RuntimeType::from_env().is_kubernetes() {
        let k8s_limits = state
            .config
            .kubernetes_config
            .as_ref()
            .and_then(|kc| kc.get_service_config(service_type))
            .map(|sc| sc.resource_limits.clone());
        match k8s_limits {
            Some(l) => (Some(l), "k8s_config"),
            None => {
                error!(
                    "[RESOURCE_LIMITS] K8s 模式下 kubernetes_config 未配置 service {:?} 的 \
                     resource_limits,拒绝降级到 docker_config(完全分家)。请在 config.yml 的 \
                     kubernetes_config.services 段补全该 service。",
                    service_type
                );
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    format!(
                        "K8s mode requires resource_limits for service {service_type:?} in kubernetes_config.services"
                    ),
                ));
            }
        }
    } else {
        // 注意：get_multi_image_config 返回 owned MultiImageConfig，需先绑定再借用，
        // clone 出 owned ServiceResourceLimits，避免悬垂引用。
        let docker_limits = state.config.docker_config.as_ref().and_then(|dc| {
            let multi_config = dc.get_multi_image_config();
            multi_config
                .get_service_config(service_type)
                .map(|c| c.resource_limits.clone())
        });
        (docker_limits, "docker_config")
    };

    // 来源标记，便于排查“资源限制静默丢失”问题（none=Pod 将无 resources，需警惕）
    let source = match (&default_limits, &api_limits) {
        (Some(_), Some(_)) => format!("merged(api+{default_source})"),
        (Some(_), None) => default_source.to_string(),
        (None, Some(_)) => "api".to_string(),
        (None, None) => "none".to_string(),
    };

    // 字段级合并：API 字段优先，None 回退默认值
    let result = match (default_limits, api_limits) {
        (Some(default), Some(api)) => Some(default.merge_with(&api)),
        (Some(default), None) => Some(default),
        (None, api) => api,
    };

    // swap 规整:swap < memory 时自动上调到 memory × 2(上游误传兜底,见
    // ServiceResourceLimits::normalize_swap)。在 merge 之后做,基于最终生效值判断。
    let (result, swap_fixed) = match result {
        Some(rl) => {
            let (fixed, changed) = rl.normalize_swap();
            (Some(fixed), changed)
        }
        None => (None, false),
    };
    if swap_fixed && let Some(ref r) = result {
        warn!(
            "[RESOURCE_LIMITS] service_type={:?}: swap < memory,自动修正 swap = memory × 2 \
             (memory={:.1}Gi → swap={:.1}Gi)",
            service_type,
            r.memory.unwrap_or(0.0) / 1024.0 / 1024.0 / 1024.0,
            r.swap.unwrap_or(0.0) / 1024.0 / 1024.0 / 1024.0,
        );
    }

    // 记录最终生效的 memory/cpu（仅这两个字段进 K8s container resources；
    // swap_limit/storage_size 不进 container resources，故不在此记录）
    let mem = result
        .as_ref()
        .and_then(|l| l.memory)
        .map(|b| format!("{:.1}Gi", b / 1024.0 / 1024.0 / 1024.0));
    let cpu = result.as_ref().and_then(|l| l.cpu);
    info!(
        "[RESOURCE_LIMITS] service_type={:?}, source={}, memory={}, cpu={}",
        service_type,
        source,
        mem.as_deref().unwrap_or("none"),
        cpu.map(|c| c.to_string()).as_deref().unwrap_or("none"),
    );

    Ok(result)
}

/// 解析 service_type 字符串为 ServiceType 枚举
///
/// 默认返回 ComputerAgentRunner（保持向后兼容）
///
/// # 参数
/// * `raw` - 原始 service_type 字符串
///
/// # 返回
/// Ok(ServiceType) 解析成功，Err(String) 解析失败
pub(super) fn parse_service_type(raw: Option<&str>) -> Result<ServiceType, String> {
    match raw {
        None | Some("") => Ok(ServiceType::ComputerAgentRunner),
        Some(s) => s
            .parse::<ServiceType>()
            .map_err(|e| format!("invalid service_type: {}", e)),
    }
}

/// 根据 ServiceType 确定容器标识符
///
/// - WebAgentRunner: 使用 project_id
/// - ComputerAgentRunner: 使用 user_id (或 pod_id)
///
/// # 参数
/// * `service_type` - 服务类型
/// * `user_id` - 用户 ID
/// * `project_id` - 项目 ID
/// * `pod_id` - 容器 ID (可选，优先级最高)
///
/// # 返回
/// 容器标识符字符串
pub(super) fn container_identifier_for_service(
    service_type: &ServiceType,
    user_id: &str,
    project_id: &str,
    pod_id: Option<&str>,
) -> Result<String, AppError> {
    // 复用 ServiceType::container_identifier（单一事实源）。调用方（pod_ensure/keepalive/
    // restart）已校验 user_id/project_id 非空，正常不会 Err。若仍缺字段 → 上游校验逻辑有
    // bug，Fail Fast 返回 500 暴露（不再用 expect panic，符合禁生产 panic 约束）。
    service_type
        .container_identifier(pod_id, Some(user_id), Some(project_id))
        .map(|id| id.to_string())
        .map_err(|e| {
            AppError::internal_server_error(&format!(
                "container_identifier for {service_type:?} failed: {e} \
                 (user_id/project_id should have been validated upstream)"
            ))
        })
}

// validate_k8s_storage_size 已下沉到 container-runtime-api（共享，避免双份维护）

/// 将 Unix 毫秒时间戳转换为东八区（UTC+8）时间字符串
///
/// # 参数
/// * `timestamp_millis` - Unix 毫秒时间戳
///
/// # 返回
/// 格式为 "YYYY-MM-DD HH:MM:SS" 的时间字符串
pub(super) fn timestamp_to_utc8_string(timestamp_millis: u64) -> String {
    use chrono::{DateTime, FixedOffset};

    // 直接从毫秒时间戳创建 DateTime<Utc>
    let datetime =
        DateTime::from_timestamp_millis(timestamp_millis as i64).unwrap_or(DateTime::UNIX_EPOCH);

    // 创建东八区时区偏移 (UTC+8)。chrono 弃用了不返回 Option 的 east()、改推 east_opt()，
    // 但 8*3600=28800 在 ±86400 内恒有效，east_opt 必返回 Some；这里用 east() 避免
    // unwrap_or_else(unreachable!)/expect 的 panic 路径，对该 let 放行 deprecation。
    #[allow(deprecated)]
    let utc8_offset = FixedOffset::east(8 * 3600);

    // 转换为东八区时间并格式化
    datetime
        .with_timezone(&utc8_offset)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

// ============================================================================
// userApp 分派（app_id / app_stage 入参）
// ============================================================================

/// pod 接口族的 userApp 分派目标。
pub(super) enum AppTarget {
    /// 无 app_id——走既有 agent/computer 路径
    NotApp,
    /// 开发容器（UserAppBuilder：虚拟终端/文件服务/PG 全套开发栈）
    Dev(String),
    /// 生产 Deployment（AppService 托管）
    Prod(String),
}

/// 解析 userApp 分派目标。
///
/// 校验规则：
/// - `app_id` 与 `service_type` 互斥（userApp 容器类型由 `app_stage` 推导，防双头语义）
/// - `app_stage` 依附于 `app_id`（单独出现视为无效）
/// - `app_id` 过 identifier 白名单（进入容器名/bind 路径拼接，防注入）
pub(super) fn parse_app_target(
    app_id: Option<&str>,
    app_stage: Option<&str>,
    service_type: Option<&str>,
) -> Result<AppTarget, String> {
    let Some(app_id) = app_id.map(str::trim).filter(|s| !s.is_empty()) else {
        if app_stage.is_some_and(|s| !s.trim().is_empty()) {
            return Err("app_stage requires app_id".to_string());
        }
        return Ok(AppTarget::NotApp);
    };
    if service_type.is_some_and(|s| !s.trim().is_empty()) {
        return Err(
            "app_id and service_type are mutually exclusive (userApp 容器类型由 app_stage 推导)"
                .to_string(),
        );
    }
    shared_types::validate_identifier(app_id, "app_id").map_err(|e| e.to_string())?;
    match app_stage
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("dev")
    {
        "dev" => Ok(AppTarget::Dev(app_id.to_owned())),
        "prod" => Ok(AppTarget::Prod(app_id.to_owned())),
        other => Err(format!(
            "invalid app_stage {other:?}: expected \"dev\" or \"prod\""
        )),
    }
}

/// userApp 分派参数校验失败的统一响应（ensure/keepalive/restart 三 handler 复用）。
pub(super) fn invalid_app_target_response<T>(locale: &str, e: &str) -> HttpResult<T> {
    HttpResult::error_with_message(shared_types::error_codes::ERR_VALIDATION, locale, e)
}
