//! agent-runner 容器创建 + PodSpec 构造（从 k8s_agent_pod.rs 拆出）。
//!
//! - `create_agent_container`：创建编排（PVC ensure → STS(replicas=1) → 等 Ready → ClusterIP svc）。
//! - `build_agent_pod_spec`：纯 PodSpec 构造（image/volumes/env/command/probes/lifecycle/sidecar），无副作用。
//!
//! 与 k8s_agent_query.rs（读）、k8s_agent_pod.rs（stop/restart/sync/cleanup 变更）正交。

use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    WorkspaceRuntime,
};
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar, LocalObjectReference,
    PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, Probe, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use shared_types::{
    ContainerBasicInfo, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, ServiceType,
};
use tracing::info;

use super::k8s_app_helpers::build_hostname_spread_constraint;
use super::k8s_pod::K8sPodOps;
use super::k8s_pvc::K8sPvcOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::{KubernetesRuntime, POD_CACHE_TTL};

impl KubernetesRuntime {
    /// 创建 agent-runner 容器（走 StatefulSet，K8s 原生 pod 级自愈）。
    ///
    /// 编排顺序：identifier 解析 → per-agent PVC ensure（副作用）→ cache-hit 早返回 →
    /// `build_agent_pod_spec` 纯构造 → headless svc + STS(replicas=1) → 等 Ready →
    /// ClusterIP svc → 取 info。Userapp 不走此路径（用 create_deployment）。
    pub(crate) async fn create_agent_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.create_agent_container_with_creation_receipt(params, None)
            .await
    }

    pub(super) async fn create_agent_container_with_creation_receipt(
        &self,
        params: ContainerCreateParams,
        creation_lease: Option<shared_types::UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        if params.execution_context.is_some() && creation_lease.is_none() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Managed builder creation requires its original lease receipt".into(),
            ));
        }
        let cancelled = params.creation_cancelled.clone();
        let check_cancelled = || {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                Err(ContainerRuntimeError::CreationCancelled)
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
        // 确定容器标识符（复用 ServiceType::container_identifier 单一事实源，
        // 与 docker 模式 / handler 层保持一致）。identifier 借自 pod_id/user_id/project_id 之一。
        // ⚠️ 不要在此重写优先级逻辑，否则会与 handler 层不一致 → ensure/chat 造出不同名 pod+PVC。
        let service_type = params.service_type;
        let project_id = params.project_id.clone();
        let user_id = params.user_id.clone();
        let pod_id = params.pod_id.clone();

        let identifier: &str = service_type
            .container_identifier(pod_id.as_deref(), user_id.as_deref(), project_id.as_deref())
            .map_err(|e| ContainerRuntimeError::ConfigurationError(e.to_string()))?;

        // Pod 名称：统一使用 pod_name() helper（含 RFC 1123 下划线清理）
        let pod_name = self.pod_name(identifier, &service_type)?;

        // A new admitted ensure invalidates old, captured DELETE versions before
        // it reuses a controller, Pod or stable Service. Only top-level metadata
        // changes; this must never roll the running Pod template.
        let previous = self
            .capture_agent_retirement(identifier, &service_type)
            .await?;
        if previous.controller.is_some()
            || !previous.pods.is_empty()
            || !previous.services.is_empty()
        {
            self.fence_agent_snapshot(&previous).await?;
        }

        // per_agent_volume 决策(async): pod_id=None 且开关开; RBD 变体额外
        // 要求 migrated 标记(预同步期 PVC 已存在但用户仍在共享卷写, agent
        // 此刻挂 RBD 旧副本会丢写); 族限定 computer(Userapp 域有自己的卷逻辑)。
        let per_agent_volume = if params.pod_id.is_some() {
            false
        } else if shared_types::per_agent_pvc_enabled() {
            true
        } else if shared_types::per_agent_rbd_enabled()
            && matches!(
                service_type,
                ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
            )
        {
            use container_runtime_api::WorkspaceRuntime;
            match self.workspace_migration_state(identifier, &service_type).await? {
                shared_types::WorkspaceMigrationState::Migrated => true,
                shared_types::WorkspaceMigrationState::NotMigrated => {
                    // 新用户初始化（终审 F8 + 复审 P2）：flag 开 + 共享卷无该
                    // 用户存量 → 直接建 RBD PVC 并写 Migrated（新用户=直上
                    // RBD，plan §P0 目标）。判据四象限：
                    // - PVC 缺 + 共享 Absent：真新用户，建卷+标记（两步）。
                    // - PVC 在 + 共享 NonEmpty：phase-a 预同步用户（有数据才
                    //   迁移），保持共享路径。
                    // - PVC 在 + 共享 Absent：只能是"新用户初始化中断"（首建
                    //   卷成功、写 Migrated 前中断）——重放 set_migrated 幂等
                    //   自愈，不得误判预同步把新用户留在共享卷（复审 P2）。
                    // - 共享 NonEmpty 而无卷：存量用户，须显式两阶段迁移
                    //   （下方 legacy 防护同语义）。
                    // PVC 读失败原样上抛（fail-fast，不猜——暂态错误下误判
                    // "真新用户"会让存量用户被切到空卷）。
                    let pvc = self
                        .pvcs()
                        .get_opt(
                            &self.workspace_pvc_name(identifier, &service_type)?,
                        )
                        .await
                        .map_err(|error| {
                            ContainerRuntimeError::ConnectionError(format!(
                                "cannot read workspace PVC state for '{identifier}': {error}"
                            ))
                        })?;
                    let volume_origin = pvc.as_ref().and_then(|existing| {
                        existing
                            .metadata
                            .labels
                            .as_ref()
                            .and_then(|labels| labels.get(crate::runtime::k8s_pvc::VOLUME_ORIGIN_LABEL))
                            .cloned()
                    });
                    // 写排空后探测（复审第五轮 P1）：受理中的共享写尚未创建
                    // 目录时，盲探测会误判 Absent——先关该用户的新写（begin，
                    // 与 intercept 的 admit_write 同一把锁），排空在途写，再
                    // 重新探测共享目录。与 cutover 的编排锁互斥保证 begin/end
                    // 不交叉。排空超时（10s 内在途写未归零）上抛可重试错误，
                    // 本次保持现状（不切卷）。
                    // RAII 门禁（第七轮复审 P1）：编排 future 在任意 await 点被取消
                    // 也会经 Drop 释放——手写 end 无法覆盖取消路径（曾致写准入
                    // 永久冻结的变体）。已派发但结果未知的 K8s 写由 ensure 幂等
                    // + 四象限重探测天然对账。
                    let mut migration_gate =
                        Some(shared_types::migration_gate::begin_guarded(identifier));
                    let drain_left = shared_types::migration_gate::drain_user_writes(
                        identifier,
                        std::time::Duration::from_secs(10),
                    )
                    .await;
                    if drain_left > 0 {
                        return Err(ContainerRuntimeError::ConnectionError(format!(
                            "in-flight writes for '{identifier}' did not drain within 10s \
                             (left={drain_left}); refusing fresh-init decision, retry later"
                        )));
                    }
                    let shared_state = probe_shared_dir(identifier).await;
                    // 探测异常时也走统一收尾路径（下方 ProbeFailed 分支 return 前
                    // 先 end，恢复该用户可写）。
                    let fresh_init_decision = match (pvc.is_some(), volume_origin.as_deref(), shared_state) {
                        // phase-a 预同步卷(共享卷有存量/无来源标) 或 探测不可
                        // 信: 保持共享路径。phase-a 与存量卷不打 fresh-init 标,
                        // rolled-back 卷显式排除在续接之外(复审第四轮 P1: 回滚
                        // 后的 ensure 不得撤销刚写入的切回决定)。
                        (true, _, SharedDirProbe::NonEmpty)
                        | (true, _, SharedDirProbe::ProbeFailed)
                        | (false, _, SharedDirProbe::NonEmpty) => false,
                        // 真新用户: 初始化直上 RBD（fresh-init 标记来源）
                        (false, _, SharedDirProbe::Absent) => {
                        // begin 之后的所有失败路径必须先 end 再上抛——
                        // `?` 直接 return 会跳过收尾，该用户写准入永久冻结。
                        if let Err(error) = self
                            .ensure_fresh_workspace_pvc(
                                identifier,
                                &service_type,
                                params.storage_size.as_deref(),
                            )
                            .await
                        {
                            return Err(error);
                        }
                        marker_completer(self, migration_gate.take(), identifier, &service_type).await?;
                            info!(
                                "[K8S] new user '{identifier}' initialized on per-agent RBD"
                            );
                            true
                        }
                        // 首建中断的自愈: **只认 fresh-init 来源标**——卷确由
                        // 新用户首建路径创建且 rollback 未改写标记, 重放
                        // Migrated 完成初始化(幂等)。无标卷(phase-a 残留/同步
                        // 失败留卷/存量卷)与 rolled-back 卷一律走共享, 不凭
                        // "空目录"推断卷的来历。
                        (true, Some(crate::runtime::k8s_pvc::VOLUME_ORIGIN_FRESH_INIT), SharedDirProbe::Absent) => {
                            marker_completer(self, migration_gate.take(), identifier, &service_type).await?;
                            info!(
                                "[K8S] new user '{identifier}' RBD init resumed (fresh-init volume, marker replayed)"
                            );
                            true
                        }
                        // 有卷但无 fresh-init 凭据 + 共享 Absent: 保守共享路径
                        // (phase-a 残留 / rolled-back / 存量卷的统一归宿)。
                        (true, _, SharedDirProbe::Absent) => false,
                        // 探测不可信(env 未设/IO 错误/共享根缺失): fail-closed
                        // 拒绝初始化——误判新用户会让存量用户数据"消失"(空
                        // RBD 卷+存在即转发)。上抛可重试错误, 不静默回落。
                        (false, _, SharedDirProbe::ProbeFailed) => {
                            return Err(ContainerRuntimeError::ConnectionError(format!(
                                "cannot probe shared workspace for '{identifier}' \
                                 (COMPUTER_WORKSPACE_DIR unset, IO error or shared root missing); \
                                 refusing new-user RBD initialization"
                            )));
                        }
                    };
                    // 决策完成（_migration_guard 随作用域 drop 自动恢复写准入）。
                    // 两条成功路径（新建/续接）的建卷+翻标记都发生在门禁内——
                    // 与文件写路由串行化。
                    fresh_init_decision
                }
            }
        } else {
            false
        };
        // 阶段2 per-agent PVC (CephFS subvolume, ceph-csi 服务端配额, 绕开 client setfattr):
        // 仅隔离容器 (pod_id=None, project/user 级) 且 per-agent 开关开启走 per-agent PVC。
        // 两变体: per_agent_pvc(subvolume, 挂根聚合读) / per_agent_rbd(RWO 块盘, 转发层读,
        // 二者互斥 fail-fast 见 FeatureFlags::init)。共享容器 (pod_id=Some) 或开关全关 → 共享 PVC。
        // UserappBuilder 天然 per-app PVC(不受灰度开关限制),总是 ensure。
        if per_agent_volume || matches!(service_type, ServiceType::UserappBuilder) {
            if params.retained_storage.is_some() {
                // recover_retained_storage already verified exact UID and ownership.
            } else if let Some(context) = &params.execution_context {
                self.ensure_owned_workspace_pvc(
                    context,
                    &service_type,
                    params.storage_size.as_deref(),
                )
                .await?;
            } else {
                // P0 防护(仅 NotMigrated 存量用户): flag 开但用户未迁移时,
                // ensure 会建**空** RBD PVC——混住判据(存在即转发)立即把请求
                // 送进空卷, 共享卷老数据静默不可见。正确流程先跑两阶段迁移。
                // ⚠️ 只在 PVC 不存在时探测(终审修正): per_agent_volume=true
                // 蕴含 Migrated, 其共享卷回滚副本必然非空——对 Migrated 用户
                // 探测 = 每次 cutover 第 5 步 ensure 必误报 Conflict 且无法
                // 自愈(状态已翻, 停机残留)。
                if shared_types::per_agent_rbd_enabled()
                    && matches!(
                        service_type,
                        ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
                    )
                    && matches!(probe_shared_dir(identifier).await, SharedDirProbe::NonEmpty)
                {
                    // PVC 已存在(phase-a 建过/已迁移) = 流程内用户, 不拦;
                    // PVC 缺失 + 共享卷非空 = 未迁移存量用户误触, 拒绝。
                    let pvc_exists = self
                        .pvcs()
                        .get_opt(&self.workspace_pvc_name(identifier, &service_type)?)
                        .await
                        .map(|p| p.is_some())
                        .unwrap_or(false);
                    if !pvc_exists {
                        return Err(ContainerRuntimeError::Conflict(format!(
                            "legacy shared workspace for '{identifier}' is non-empty and no \
                             per-agent PVC exists; run phase-a + cutover before creating \
                             per-agent RBD volume"
                        )));
                    }
                }
                self.ensure_workspace_pvc(
                    identifier,
                    &service_type,
                    params.storage_size.as_deref(),
                )
                .await?;
            }
        }

        check_cancelled()?;
        if service_type == ServiceType::UserappBuilder {
            self.claim_builder_storage_with_context(identifier, params.execution_context.as_ref())
                .await?;
        }

        // Check if pod already exists and is running.
        // 读守卫物化到独立块内结束（scrutinee 临时值在 Rust 里存活到整个 if 语句
        // 结束——即便 .cloned() 只解决 entry 的数据借用，guard 仍会跨下方 await；
        // 若 body 内再入 pod_cache 写锁即自死锁，tokio RwLock 不可升级）。
        // 类型校验：同 identifier 异族条目（如生产 UserApp）视为不存在。
        let cached_running = {
            let entry = self.pod_cache.read().await.get(identifier).cloned();
            entry.is_some_and(|entry| {
                entry.cached_at.elapsed() < POD_CACHE_TTL
                    // cache 恒存族代表值，查询侧归一后字面比较（本义查询同命中）
                    && entry.service_type == service_type.family_representative()
                    && entry.info.status == ContainerRuntimeStatus::Running
            })
        };
        if cached_running && params.execution_context.is_none() {
            info!("[K8S] Pod {} already exists and is running", pod_name);
            // Creation owns a mutation lease: service reconciliation errors must
            // reach its completion policy, not be swallowed by read-side self-heal.
            self.create_agent_service(identifier, &service_type).await?;
            return self
                .get_container_info_inner(identifier, &service_type)
                .await?
                .ok_or_else(|| ContainerRuntimeError::ContainerNotFound(identifier.to_string()));
        }

        check_cancelled()?;
        // 构造 PodSpec（纯计算，无 K8s API 副作用）。
        // Pod 的 ObjectMeta（name/labels）不传给 STS——STS 模板的 labels 由
        // build_agent_statefulset 经 build_standard_labels 自行设置，故此处只返回 PodSpec。
        let credential = if service_type == ServiceType::UserappBuilder {
            let existing = self
                .statefulsets()
                .get_opt(&pod_name)
                .await
                .map_err(|error| {
                    ContainerRuntimeError::K8sError(format!(
                        "Read builder credential source: {error}"
                    ))
                })?;
            let retained = existing
                .as_ref()
                .and_then(|sts| sts.spec.as_ref())
                .and_then(|spec| spec.template.spec.as_ref())
                .map(super::k8s_builder_control::retained_builder_credential)
                .transpose()?
                .flatten();
            retained.or_else(|| {
                existing.is_none().then(|| EnvVar {
                    name: "APP_CLI_DEPLOY_TOKEN".into(),
                    value: Some(uuid::Uuid::new_v4().simple().to_string()),
                    ..Default::default()
                })
            })
        } else {
            None
        };
        let mut pod_spec =
            self.build_agent_pod_spec(identifier, &service_type, &params, per_agent_volume)?;
        if let Some(credential) = credential {
            Self::install_builder_credential(&mut pod_spec, credential)?;
        }

        // agent-runner 走 StatefulSet（K8s 原生 pod 级自愈）：把 pod spec
        // wrap 进 STS（而非裸 Pod）。STS replicas=1 时 pod 被 evict/删除 → 控制器自动重建
        // 同名 pod（挂回同 PVC，数据不丢）；容器级 OOM 仍由 restartPolicy=Always 原地重启。
        // service_type 重名/不匹配由 ensure_agent_statefulset 内部删旧重建处理。
        self.ensure_agent_headless_service(identifier, &service_type)
            .await?;
        check_cancelled()?;
        if let Some(context) = params.execution_context.as_ref() {
            if service_type != ServiceType::UserappBuilder {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Application context requires builder StatefulSet".into(),
                ));
            }
            self.ensure_builder_statefulset(context, pod_spec).await?;
        } else {
            self.ensure_agent_statefulset(identifier, &service_type, pod_spec, 1)
                .await?;
        }

        // Wait for pod to be ready
        check_cancelled()?;
        tokio::select! {
            result = self.wait_for_pod_ready(identifier, &service_type) => result?,
            _ = async {
                loop {
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } => return Err(ContainerRuntimeError::CreationCancelled),
        }
        check_cancelled()?;
        if service_type == ServiceType::UserappBuilder {
            // Pod 就绪后的管理面恢复：对上一物理 Pod 遗留的执行代次做平台核验
            // （失败只保留保护并记原因，不阻塞 ensure）。
            self.reconcile_builder_execution_domain_bounded(identifier)
                .await;
        }

        // Host-side address materialization needs the Service's assigned
        // NodePorts. Keep the admitted builder lease while creating it; the
        // completion receipt remains a separate final, identity-checked
        // Service mutation below. A crash between these steps leaves a
        // discoverable Service without a success receipt.
        self.create_agent_service(identifier, &service_type).await?;
        let info = self
            .get_container_info_inner(identifier, &service_type)
            .await?
            .ok_or_else(|| {
                ContainerRuntimeError::ContainerCreationError(
                    "Pod created but info not found".into(),
                )
            })?;
        if let (Some(context), Some(lease)) = (&params.execution_context, creation_lease) {
            let target = self
                .capture_builder_compute_with_binding(
                    context,
                    params.resource_binding.as_ref(),
                    false,
                )
                .await?;
            let receipt = super::builder_creation_receipt::BuilderCreationReceipt {
                target,
                container: info.clone(),
                lease,
            };
            // The final resource mutation carries its own acknowledgement.
            // Nothing after this call creates, scales or patches compute.
            check_cancelled()?;
            self.commit_builder_service_creation(&receipt).await?;
        }
        Ok(info)
    }

    /// 纯构造 agent-runner 的 PodSpec（image 选择、PVC 名、volumes/mounts 翻译、env 合并、
    /// command、probe、resources、sidecar）。
    ///
    /// 只读 `self.config` + `&params`，无 K8s API 副作用，自包含可单测。
    /// PodSpec 字面量与原 create_container 内联构造逐字一致。
    pub(crate) fn build_agent_pod_spec(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        params: &ContainerCreateParams,
        per_agent_volume: bool,
    ) -> ContainerRuntimeResult<PodSpec> {
        let project_id_val = params.project_id.clone().unwrap_or_default();
        let user_id_val = params.user_id.clone().unwrap_or_default();
        // 家族归一：容器身份/环境（SERVICE_TYPE env 等）按家族值——agent_runner
        // 的 cwd 由 gRPC service_type 决定，不消费该 env
        let service_type_str = service_type.container_family_key().to_string();
        let image = if *service_type == ServiceType::UserappBuilder {
            params
                .image_override
                .clone()
                .unwrap_or_else(|| self.select_image(service_type))
        } else {
            self.select_image(service_type)
        };

        // Build resource requirements if limits are provided; agent 族容器
        // （ensure/chat 等全部创建入口）未显式传入时用统一常量默认
        // （6Gi/2 核，见 shared_types::DEFAULT_AGENT_*）——不给环境留隐式套
        // 上限的空间（131 实证 547Mi 隐式上限 → 容器冻结）。
        let resources = match params.resource_limits.as_ref() {
            Some(limits) => Self::build_resource_requirements(limits),
            None => Self::build_resource_requirements(&shared_types::ServiceResourceLimits {
                memory: Some(shared_types::DEFAULT_AGENT_MEMORY_LIMIT_BYTES),
                cpu: Some(shared_types::DEFAULT_AGENT_CPU_LIMIT_CORES),
                swap: None,
                ephemeral_storage_limit: None,
                storage_size: None,
            }),
        };

        // workspace PVC:
        // - per-agent (pod_id=None + 任一 per-agent 开关): per-agent PVC (subPath=None)
        // - 共享 (pod_id=Some 或开关全关): 共享 PVC + subPath (选项A / 回滚)
        // ⚠️ per_agent_rbd 还要求 migrated 标记: phase-a 预同步期 PVC 已存在
        // 但用户仍在共享卷写——此刻 agent 重建若挂 RBD 旧副本, 预同步后的写
        // 全丢。未 migrated → 继续共享路径, 等 cutover 翻标记后下次重建才切。
        // per_agent_volume 由调用方算定传入: RBD 变体的 migrated 查询是 async
        // (PVC annotation), 本函数是纯同步 PodSpec 构造, 不能在此 await。
        let per_agent = per_agent_volume;
        let (workspace_pvc, workspace_sub_path): (String, Option<String>) = if per_agent {
            (self.workspace_pvc_name(identifier, service_type)?, None)
        } else {
            match service_type {
                ServiceType::WebAgentRunner => (
                    std::env::var("RCODER_WORKSPACE_PVC_NAME")
                        .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace)),
                    Some(
                        std::env::var("RCODER_WORKSPACE_SUBPATH")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "workspace".to_string()),
                    ),
                ),
                ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => (
                    std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME").unwrap_or_else(|_| {
                        format!("{}-rcoder-computer-workspace", self.namespace)
                    }),
                    Some(user_id_val.clone()),
                ),
                // Userapp 实际走 create_deployment（k8s_app_create），不经此路径
                // （防御性兜底）；UserappBuilder 显式列出防漏臂
                ServiceType::Userapp | ServiceType::UserappBuilder => {
                    (self.workspace_pvc_name(identifier, service_type)?, None)
                }
            }
        };

        // (阶段2: xattr 目录配额已退役 —— 改用 per-agent subvolume PVC + CSI 服务端配额。
        //  parse_quantity_to_bytes / agent_workspace_quota_dir / xattr crate 已删, 见 Task 2.3)

        // 取 service 配置(完全分家):K8s 优先读 kubernetes_config;docker_config.multi_image_config
        // 仅作过渡期安全兜底(旧 chart 未带 kubernetes_config 段时,保留 workspace 路径/command/env 行为)。
        // volumes / volume_mounts / sidecars 只来自 kubernetes_config(docker_config 无此概念)。
        let k8s_service = self
            .config
            .kubernetes_config
            .get_service_config(service_type);
        let docker_service = self
            .config
            .docker_manager_config
            .multi_image_config
            .get_service_config(service_type);

        // workspace 挂载路径(K8s 模式 computer→/home/user, web→/app/project_workspace)。
        // UserappBuilder 不走 docker fallback：docker 段的 workspace_resolution_path 是
        // rcoder 容器视角的宿主反解锚点（/app/userapp-workspace），K8s 下误用会把
        // PVC 挂到 overlay 路径造成数据面分裂——k8s 配置缺失时直接用镜像契约默认。
        // （UserappBuilder 的实际挂载不走此单挂载点——见下方 volume_mounts 的
        //  四 subPath 压平分支，此值对 builder 不生效。）
        let workspace_mount_path = k8s_service
            .map(|sc| sc.workspace_container_path())
            .or_else(|| {
                if matches!(service_type, ServiceType::UserappBuilder) {
                    None
                } else {
                    docker_service.map(|sc| sc.workspace_container_path())
                }
            })
            .unwrap_or_else(|| match service_type {
                // 常规项目与 Computer 共容器，挂载点同源常量（见 paths.rs
                // COMPUTER_AGENT_MOUNT_ROOT 与 WORKSPACE_ROOT 的视角说明）
                ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                    shared_types::paths::COMPUTER_AGENT_MOUNT_ROOT.to_string()
                }
                // Userapp 域 per-app 工作区定位 {USERAPP_WORKSPACE_ROOT}/{app_id}
                // （容器内 userapp 共享卷根，builder 与生产容器同源）。UserappBuilder
                // 挂载压平（四 subPath 分支在 volume_mounts 构造处短路）；Userapp
                // 实际走 create_deployment 不经此路径——此兜底值均为 match
                // 完备性保留，但语义须正确（勿落 web 的 /app/project_workspace）
                ServiceType::Userapp | ServiceType::UserappBuilder => {
                    shared_types::paths::USERAPP_WORKSPACE_ROOT.to_string()
                }
                ServiceType::WebAgentRunner => shared_types::paths::WORKSPACE_ROOT.to_string(),
            });

        // 构建 volumes: 硬编码 workspace PVC(保留) + 翻译 kubernetes_config 额外卷
        let mut volumes_vec: Vec<Volume> = vec![Volume {
            name: "workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_pvc.clone(),
                read_only: Some(false),
            }),
            ..Default::default()
        }];
        let extra_volumes: Vec<K8sVolumeSpec> =
            k8s_service.map(|s| s.volumes.clone()).unwrap_or_default();
        for v in extra_volumes.iter().flat_map(Self::translate_k8s_volume) {
            volumes_vec.push(v);
        }
        if matches!(service_type, ServiceType::UserappBuilder) {
            // 固定只读平台绑定（recovery v2 plan §6.1）：Downward API 投放
            // pod UID + 执行域注解；容器身份不再依赖 spawn 链透传 env。
            volumes_vec.push(super::k8s_native_domain::platform_binding_volume());
        }

        // 构建 volume_mounts: workspace 挂载 + 翻译 kubernetes_config 额外挂载(挂到 agent 容器)
        let mut volume_mounts_vec: Vec<VolumeMount> =
            if matches!(service_type, ServiceType::UserappBuilder) {
                // UserappBuilder 开发容器: 同一块 per-app RWO PVC 的四个 subPath 视图
                // （挂载压平）。卷内布局 {app_id}/ + data/ + logs/ + agent-store/
                // （一卷一 app 拍平, env/user 层只存在于宿主/卷内, 容器内不体现）;
                // 容器内路径是平台契约（file-server USERAPP_WORKSPACE_DIR / PGDATA /
                // DBX_DATA_DIR env 均按此注入; .agent-store 挂载点与 agent_store.rs
                // 的 user_root/.agent-store 契约对齐——workspace 内相对软链
                // `../.agent-store/...` 恰好解析到该挂载点）, 不读
                // workspace_container_path 配置——误配会造成数据面分裂（同上方排除
                // docker fallback 的理由）。subPath 目录由 kubelet 挂载时自动创建
                // （运行中勿在容器内删除, 重启后挂载点失效）。
                vec![
                    VolumeMount {
                        name: "workspace".to_string(),
                        mount_path: format!(
                            "{}/{}",
                            shared_types::paths::USERAPP_DEV_HOME,
                            identifier
                        ),
                        sub_path: Some(identifier.to_string()),
                        read_only: Some(false),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "workspace".to_string(),
                        mount_path: shared_types::paths::USERAPP_DEV_DATA.to_string(),
                        sub_path: Some("data".to_string()),
                        read_only: Some(false),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "workspace".to_string(),
                        mount_path: shared_types::paths::USERAPP_DEV_LOGS.to_string(),
                        sub_path: Some("logs".to_string()),
                        read_only: Some(false),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "workspace".to_string(),
                        mount_path: shared_types::paths::USERAPP_DEV_AGENT_STORE.to_string(),
                        sub_path: Some("agent-store".to_string()),
                        read_only: Some(false),
                        ..Default::default()
                    },
                ]
            } else {
                // computer→subPath=Some(user_id)，web→共享 PVC subPath；builder 不走此分支
                vec![VolumeMount {
                    name: "workspace".to_string(),
                    mount_path: workspace_mount_path,
                    sub_path: workspace_sub_path,
                    read_only: Some(false),
                    ..Default::default()
                }]
            };
        let extra_mounts: Vec<K8sVolumeMountSpec> = k8s_service
            .map(|s| s.volume_mounts.clone())
            .unwrap_or_default();
        for m in extra_mounts.iter().map(Self::translate_k8s_volume_mount) {
            volume_mounts_vec.push(m);
        }

        let volumes = Some(volumes_vec);
        if matches!(service_type, ServiceType::UserappBuilder) {
            volume_mounts_vec.push(VolumeMount {
                name: "rcoder-platform-binding".to_string(),
                mount_path: super::k8s_native_domain::PLATFORM_BINDING_MOUNT.to_string(),
                read_only: Some(true),
                ..Default::default()
            });
        }
        let volume_mounts = Some(volume_mounts_vec);

        // sidecar 容器(只来自 kubernetes_config):如 log-collector tail 容器内日志到 stdout
        let sidecars: Vec<K8sSidecarSpec> =
            k8s_service.map(|s| s.sidecars.clone()).unwrap_or_default();

        // Build image pull secrets if configured
        let image_pull_secrets = self.config.image_pull_secret.as_ref().map(|secret| {
            vec![LocalObjectReference {
                name: secret.clone(),
            }]
        });

        // topologySpreadConstraints：动态 agent-runner 跨节点均衡，修复"全部堆在一个节点"的
        // 调度失衡（根因：requests 50m 极低，调度器按 requests 以为节点很轻，反复选最闲节点）。
        // 策略与参数理由见 build_hostname_spread_constraint 和
        // docs/agent-runner-scheduling-balance.md。label 值必须用家族值
        // （build_standard_labels 写入的 app.kubernetes.io/name 是 container_family
        // 归一值——原始值 computer-normal-project 永远匹配不到自己的 Pod，约束失效）；
        // 常规项目与 Computer 同容器同 label，归入同臂。
        let topology_spread_constraints = match service_type {
            ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::WebAgentRunner
            | ServiceType::UserappBuilder => Some(vec![build_hostname_spread_constraint(
                service_type.container_family_key(),
            )]),
            // Userapp 实际走 create_deployment（k8s_app_create），不经此路径（防御性兜底）；
            // 其均衡在 build_app_deployment 用共享 label user-app 注入。
            ServiceType::Userapp => None,
        };

        Ok(PodSpec {
            volumes,
            image_pull_secrets,
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(false),
                ..Default::default()
            }),
            termination_grace_period_seconds: Some(15),
            containers: {
                // 主 agent 容器 + 翻译自 kubernetes_config 的 sidecar(如 log-collector)
                let mut containers_vec = vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    // IfNotPresent: 动态 pod 频繁创建（每 chat/computer-chat 一个），
                    // 节点已缓存就直接用，避免每次都去 registry 验 token/manifest。
                    // image 更新由主 Deployment 触发拉取（用户做 rollout restart 时），
                    // 主服务用新 image 启动后，动态 pod 跟着用同样的 image 引用。
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    // 启动命令：
                    //   - WebAgentRunner：从 config.yml 的 web-agent-runner.command 读取
                    //     （与 docker-compose 一致）。配置里的 /app/agent-runner-start.sh wrapper
                    //     会先 nohup 拉起 ttyd(7681)，再 exec agent_runner；agent_runner 的
                    //     ws_terminal 中间层(17681)依赖 ttyd 就绪后才会 bind。若不读配置而裸跑
                    //     agent_runner，ttyd 不启动 -> ws_terminal 等 7681 超时 abort ->
                    //     /computer/terminal 终端 WS 连不上。配置缺失时回退裸 agent_runner
                    //     （保留旧行为，至少 pod 能起；rcoder-master 镜像本身没有 CMD/ENTRYPOINT）。
                    //   - ComputerAgentRunner：刻意用 None 走镜像自带 ENTRYPOINT(start-up.sh)。
                    //     注意 config.yml 里 computer-agent-runner.command 写的是裸 agent_runner，
                    //     那是给 docker 运行时用的；K8s 下若改读它会绕过 start-up.sh，丢失 ttyd/VNC，
                    //     因此这里不复用 config.command。
                    command: match service_type {
                        ServiceType::WebAgentRunner => {
                            // 优先 kubernetes_config.command;过渡期回退 docker_config.command;
                            // 都缺则裸跑 agent_runner(保留旧行为,至少 pod 能起)。
                            let cmd = k8s_service
                                .and_then(|sc| {
                                    if sc.command.is_empty() {
                                        None
                                    } else {
                                        Some(sc.command.clone())
                                    }
                                })
                                .or_else(|| {
                                    docker_service.and_then(|sc| {
                                        if sc.command.is_empty() {
                                            None
                                        } else {
                                            Some(sc.command.clone())
                                        }
                                    })
                                })
                                .unwrap_or_else(|| vec!["/app/bin/agent_runner".to_string()]);
                            Some(cmd)
                        }
                        // ComputerAgentRunner / Userapp 用镜像自带 ENTRYPOINT/CMD
                        // (Userapp 实际走 create_deployment,不经此路径;
                        //  ComputerAgentRunner 走 start-up.sh 启 ttyd/VNC + agent_runner;
                        //  常规项目与 Computer 共享容器,同 ENTRYPOINT)
                        ServiceType::ComputerAgentRunner
                        | ServiceType::ComputerNormalProject
                        | ServiceType::Userapp => None,
                        // UserappBuilder 完整开发容器: 默认镜像 ENTRYPOINT/start-up.sh
                        // 起 agent_runner + 内嵌 file-server(60000) + PG 全套——userApp 的
                        // 文件/exec/dev-server/构建/chat 开发对话都在此容器内执行;
                        // kubernetes_config.user-app-builder.command 仍可显式覆盖。
                        ServiceType::UserappBuilder => k8s_service.and_then(|sc| {
                            if sc.command.is_empty() {
                                None
                            } else {
                                Some(sc.command.clone())
                            }
                        }),
                    },
                    env: {
                        let mut env = super::k8s_agent_env::build_agent_env_vars(
                            &project_id_val,
                            &user_id_val,
                            &service_type_str,
                            service_type,
                            (docker_service, k8s_service),
                            params,
                            None,
                            per_agent_volume,
                        );
                        if matches!(service_type, ServiceType::UserappBuilder) {
                            // 执行域身份（进程域退出回执的绑定锚点）：instance 经
                            // RCODER_PHYSICAL_POD_UID 在 Pod 内解析（调度前不可知）；
                            // volume 绑定 per-app PVC 与其挂载视图，跨 Pod 重建稳定。
                            env.push(EnvVar {
                                name: runtime_supervisor::domain::DOMAIN_ENV.to_string(),
                                value: Some(super::k8s_native_domain::builder_domain_env(
                                    &self.config.execution_authority,
                                    &workspace_pvc,
                                    volume_mounts.as_deref().unwrap_or_default(),
                                )),
                                ..Default::default()
                            });
                        }
                        Some(env)
                    },
                    ports: Some(vec![
                        ContainerPort {
                            container_port: shared_types::GRPC_DEFAULT_PORT as i32,
                            name: Some("grpc".to_string()),
                            ..Default::default()
                        },
                        // HTTP health check port for agent_runner
                        ContainerPort {
                            container_port: 8086,
                            name: Some("http".to_string()),
                            ..Default::default()
                        },
                        // file-server port (embedded, Userapp workspace build / package download)
                        ContainerPort {
                            container_port: shared_types::AGENT_FILE_SERVER_PORT as i32,
                            name: Some("file-server".to_string()),
                            ..Default::default()
                        },
                    ]),
                    resources,
                    volume_mounts,
                    liveness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/health".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(30),
                        period_seconds: Some(10),
                        // 131 实证（userapp 套件多轮）：control-plane 混部节点高负载下
                        // 8086 响应间歇 >3s（readiness 同端口 deadline exceeded 多次记录，
                        // 空闲 pod 亦出现），原 timeout=3×failure=3 在 30s 窗口内连败即
                        // SIGTERM 误杀活容器——在途连接 RST（rcoder 转发表现为
                        // IncompleteMessage），builder 链路整轮瘫痪。放宽为 10s×6（60s
                        // 宽限）：真死锁进程晚 30s 被杀无碍，误杀活容器代价高得多。
                        timeout_seconds: Some(10),
                        failure_threshold: Some(6),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    // readiness_probe: 探 /ready (检查 gRPC 50051 就绪), 与 liveness 的 /health 分离。
                    // gRPC 没起 → /ready 返 503 → pod NotReady → Service 摘流量; gRPC 起了 → 200 → 放流量。
                    // initialDelay/period 用 1s: 每 1s 探一次, /ready=200 后 ~1s 内 Ready。
                    // failure_threshold=20 容忍启动期 503 (gRPC 还没起), 不被误判 NotReady 太久。
                    readiness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/ready".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(1),
                        period_seconds: Some(1),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(20),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    // 注: 此处不配 startup_probe。
                    // 历史: 曾配 startup_probe(initialDelay=5/period=10/failure=12, ~2min 宽限) 防
                    // initdb 慢启动被 liveness 误杀; 但 bebba86 已把 PG initdb 异步化(supervisor 托管),
                    // /health 2s 内 ready, 慢启动根因消除。保留 startup_probe 反成拖累: initialDelay=5 +
                    // period=10 粒度让 Ready 卡在 ~11s(应用早 ready 却要等首探 pod+5s)。去掉后
                    // readiness(period=1) 直接接管, ~3s 内 Ready; 启动期保护由 liveness 兜底
                    // (initialDelay=30 + failure=3×period=10 = 50s 宽限, 远大于 2s ready, 不会被误杀)。
                    // 若日后 agent-runner 首启又变慢(>50s), 再考虑重新引入激进配置的 startup probe。
                    // preStop lifecycle hook: 在 kubelet 发送 SIGTERM 之前执行，
                    // 确保 JuiceFS FUSE 卷上的写入 buffer flush 到磁盘，
                    // 减少 FUSE unmount 卡住的概率
                    lifecycle: Some(k8s_openapi::api::core::v1::Lifecycle {
                        pre_stop: Some(k8s_openapi::api::core::v1::LifecycleHandler {
                            exec: Some(k8s_openapi::api::core::v1::ExecAction {
                                command: Some(vec![
                                    "sh".to_string(),
                                    "-c".to_string(),
                                    "sync && sleep 2".to_string(),
                                ]),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    // 启用 TTY + stdin: 让 k9s / kubectl exec -it 能进交互式 shell 排查。
                    // agent_runner 是服务进程(PID 1 监听 8086/50051, 不读 stdin), tty 不影响其运行;
                    // 与 Docker 模式(container_creator.rs tty:true)对齐。
                    // 副作用: PTY 下 agent 容器 stdout/stderr 合并成一条流(loki stream 标记失效),
                    // 但 agent_runner 同时写文件日志(/app/logs, 经 log-collector sidecar 进 loki, 完整),
                    // 故日志排查不受影响。
                    tty: Some(true),
                    stdin: Some(true),
                    ..Default::default()
                }];
                // sidecar(只来自 kubernetes_config.services[].sidecars)。
                // 无配置时 pod = 仅 agent(干净基线)。log-collector 等采集器在 configmap 声明。
                containers_vec.extend(sidecars.iter().map(Self::translate_k8s_sidecar));
                containers_vec
            },
            // Always(非 Never): agent 容器 OOM/崩溃时由 kubelet 原地重启自愈。
            // Never 下 agent 一死(sidecar 还活着 → pod 仍 Running)rcoder 既不重启也不重建 → 用户中断。
            // rcoder 的 stop/restart/destroy 均走 pods().delete() 整 pod 删, 不依赖 Never;
            // /computer/agent/stop 是 gRPC 取消会话(进程继续), 故 Always 只补崩盘自愈、不冲突。
            restart_policy: Some("Always".to_string()),
            service_account_name: Some(self.config.service_account_name.clone()),
            topology_spread_constraints,
            ..Default::default()
        })
    }

    pub(crate) fn install_builder_credential(
        pod_spec: &mut PodSpec,
        credential: EnvVar,
    ) -> ContainerRuntimeResult<()> {
        let agent = pod_spec
            .containers
            .iter_mut()
            .find(|container| container.name == "agent")
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Builder agent container missing".into())
            })?;
        let env = agent.env.get_or_insert_default();
        env.retain(|entry| entry.name != "APP_CLI_DEPLOY_TOKEN");
        env.push(credential);
        Ok(())
    }
}

/// 共享 CephFS 卷在 rcoder 容器内的挂载根（env `COMPUTER_WORKSPACE_DIR`，
/// 与 file-server LocalWorkspaceResolver 同源；未设 → None = 不探测）。
fn legacy_shared_root() -> Option<std::path::PathBuf> {
    std::env::var("COMPUTER_WORKSPACE_DIR")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// 该 identifier 的共享卷目录是否非空。NotFound → false（新用户）；
/// 其他 IO 错误 → false（fail-open：探测失败不阻断创建，迁移流程是真源）。
/// 共享卷探测三态（终审 P1）：Absent=目录不存在（真新用户）；
/// NonEmpty=有存量（存量用户）；ProbeFailed=无法探测（env 未设/IO 错误）。
/// 消费方语义相反：legacy 防护把 ProbeFailed 当"无存量"放行（fail-open，
/// 保持共享=安全方向）；F8 初始化把 ProbeFailed 上抛（fail-closed——
/// 当新用户处理会让存量用户数据"消失"，探测不可信时必须拒绝初始化）。
enum SharedDirProbe {
    Absent,
    NonEmpty,
    ProbeFailed,
}

async fn probe_shared_dir(identifier: &str) -> SharedDirProbe {
    let Some(root) = legacy_shared_root() else {
        return SharedDirProbe::ProbeFailed;
    };
    // 共享根本身缺失/不可读 = 环境配置错误或卷未挂载, 不是"该用户无数据"
    // ——按探测失败处理(fail-closed), 不能当 Absent 误判新用户(复审第四轮 P1)。
    if tokio::fs::metadata(&root).await.is_err() {
        return SharedDirProbe::ProbeFailed;
    }
    let path = root.join(identifier);
    match tokio::fs::read_dir(&path).await {
        Ok(mut rd) => match rd.next_entry().await {
            Ok(Some(_)) => SharedDirProbe::NonEmpty,
            Ok(None) => SharedDirProbe::Absent,
            Err(_) => SharedDirProbe::ProbeFailed,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SharedDirProbe::Absent,
        Err(_) => SharedDirProbe::ProbeFailed,
    }
}

/// 首建 marker PATCH 的完成者（第八轮复审 P1）：门禁所有权随任务转移——
/// 编排 future 在 PATCH 派发后被取消不再提前放行写；完成者核对同一 PVC
/// （uid+rv 前置条件）、写判据缓存后才解除门禁。此前反例：取消后门禁
/// 放行 + 服务端 PATCH 仍提交 → 缓存 NotMigrated 与 PVC Migrated 分裂，
/// 并发写落旧共享卷。
async fn marker_completer(
    runtime: &KubernetesRuntime,
    gate: Option<shared_types::migration_gate::MigrationGuard>,
    identifier: &str,
    service_type: &ServiceType,
) -> ContainerRuntimeResult<()> {
    use crate::runtime::k8s_pvc::K8sPvcOps;
    let Some(gate) = gate else {
        tracing::warn!(user_id = identifier, "marker completer called without gate");
        return Ok(());
    };
    let pvc_name = runtime.workspace_pvc_name(identifier, service_type)?;
    // 缓存键 = 物理 PVC 名（与 migration_cache_key 严格一致——第九轮 P1：
    // 此前硬编码 {service_type}/{identifier} fallback 格式，PATCH 成功却写
    // 错键，路由读不到 → 继续走旧共享卷）。
    let cache_key = runtime
        .workspace_pvc_name(identifier, service_type)
        .unwrap_or_else(|_| format!("{service_type}/{identifier}"));
    let client = runtime.client.clone();
    let namespace = runtime.namespace.clone();
    let cache = std::sync::Arc::clone(&runtime.workspace_exists_cache);
    // 主流程 await 完成者（第九轮 P1）：标记 PATCH + 判据缓存写都确认后才
    // 继续（此前派发即继续建 RBD agent，PATCH 失败只记日志）。取消安全保
    // 留：外层 future 取消只是不再等结果——spawn 的任务继续跑完并持门禁
    // 至 PATCH/缓存落定（JoinHandle drop 不取消任务）。
    let handle = tokio::spawn(async move {
        let _held = gate;
        crate::runtime::kubernetes_runtime::workspace_runtime::set_migration_state_detached(
            client,
            namespace,
            cache,
            pvc_name,
            cache_key,
            shared_types::WorkspaceMigrationState::Migrated,
        )
        .await
    });
    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(
                user_id = identifier,
                error = %error,
                "[K8S] fresh-init marker PATCH failed in completer; gate released on drop"
            );
            Err(error)
        }
        Err(error) => Err(ContainerRuntimeError::ConnectionError(format!(
            "marker completer task failed: {error}"
        ))),
    }
}
