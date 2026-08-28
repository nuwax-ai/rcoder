//! UserApp 创建链（从 service.rs 拆出，extension-impl）。
//!
//! create_app + validate/provision/runtime/assemble 创建流水。

use chrono::Utc;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 创建应用（公共入口：自动获取进程级发布锁）
    ///
    /// ⚠️ 调用方不得已持 `acquire_process_release_lock(app_id)` —— tokio Mutex
    /// 不可重入，已持锁调用会永久挂起（activate 的 ensure_app_runtime 走
    /// `create_app_locked` 内核避免此问题）。
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        // ⚠️ 调用方不得已持 `acquire_process_release_lock(app_id)` —— tokio Mutex
        // 不可重入，已持锁调用会永久挂起（activate 的 ensure_app_runtime 走
        // [`create_app_locked`] 内核避免此问题）。
        //
        // 多副本并发 create 同名 app：历史方案是 workspace 根级 flock（依赖 CephFS
        // RWX 共享挂载）——RBD 卷形态下 rcoder 不再挂卷，flock 基础不复存在，删除。
        // 现状语义：两副本并发 create 同名 → SSA force 合并（后写覆盖）。create 已
        // 从 REST 面删除（仅内部 ensure/start-deploy 路径），且 Java 六步编排串行，
        // 风险面收敛；如未来需要强互斥再引入 PG advisory lock。
        let app_id = self.validate_create_request(&request).await?;
        // 与发布流水线/delete 串行: 防发布流水线 EnsureApp 建 Deployment 与并发
        // DELETE 互踩 (删成功但 Deployment 复活/半删半建脏状态)。
        let process_lock = self.acquire_process_release_lock(&app_id).await;
        let result = self.create_app_locked(&app_id, request, process_lock).await;
        if result.is_ok() {
            self.invalidate_deploy_cache().await;
        }
        result
    }

    /// 已持锁内核：调用方持有该 app 的进程级发布锁（防止与发布流水线互踩），
    /// 本函数不再取锁。供 `create_app` 公共包装和 `ensure_app_runtime`（activate
    /// 锁内调用）共用——拆分正是为了消除 activate→ensure_app_runtime→create_app
    /// 的重入死锁。
    ///
    /// 入口统一解析默认镜像（单一收口）：image 缺省 → 平台默认运行时镜像
    /// （env `RCODER_RUNTIME_IMAGE_DIGEST`，测试/生产由部署注入；与发布链
    /// ensure_app_runtime 同源）。填充后全链路（params/AppInfo）恒 Some。
    pub(crate) async fn create_app_locked(
        &self,
        app_id: &str,
        request: CreateAppRequest,
        _process_lock: tokio::sync::OwnedMutexGuard<()>,
    ) -> AppResult<AppInfo> {
        // 默认镜像单一收口（见函数 doc）：填充后 params/AppInfo 全链路恒 Some
        let mut request = request;
        if request
            .image
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            request.image = Some(crate::runtime::params::default_runtime_image(
                &std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok(),
            )?);
        }
        info!(
            "[APP] creating app: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );
        self.provision_app_workspace(app_id, &request).await?;
        // provision_app_workspace 失败不走此分支：PVC/目录保留，下次 create 幂等复用。
        if let Err(error) = self.create_app_runtime(app_id, &request).await {
            // 部分失败兜底（create_deployment 可能已部分建成/后续步骤残留）：
            // best-effort 删除 Deployment，容忍 NotFound（create_deployment 自身失败时
            // 尚未产生部署）；清理失败仅 warn，绝不覆盖原始错误。
            if let Err(cleanup_error) = self.runtime.delete_deployment(app_id).await {
                warn!(
                    "[APP] best-effort cleanup after create_app_runtime failure: delete_deployment app_id={app_id} failed (NotFound tolerated): {cleanup_error}"
                );
            }
            return Err(error);
        }
        // 同 ID 删除后重建时，必须清除旧的 stopped/wake-blocked 内存态。
        self.activity.mark_running(app_id);
        // 业务元数据落库/缓存（name/租户/业务创建时间;集群不持有。request 随后 move 进 assemble）
        // user_id：request 显式值优先；空串=未设置（内部 ensure 构造无 user 上下文），
        // **回填已存值**——create-workspace/build 已注册的 owner 不被覆盖清空
        // （record 是整行 upsert；start-deploy 经此路径，清空会让 purge 的
        // prod/{user_id}/data 定位与 apps 代理 URL 拼接丢 owner）。
        let user_id = Some(request.user_id.clone())
            .filter(|u| !u.trim().is_empty())
            .or_else(|| self.metadata.lookup(app_id).and_then(|m| m.user_id.clone()));
        self.metadata
            .record(
                app_id,
                Some(request.name.clone()),
                user_id,
                request.tenant_id.clone(),
                request.space_id.clone(),
            )
            .await;
        Ok(self.assemble_app_info(app_id.to_string(), request).await)
    }

    /// 校验创建请求并解析 app_id（app_id 规范 + 唯一性 + 资源格式 + 端口）。
    /// 任一校验失败 Fail Fast 返回 ERR_VALIDATION / ERR_APP_ALREADY_EXISTS。
    async fn validate_create_request(&self, request: &CreateAppRequest) -> AppResult<String> {
        // user_id：归属用户（部署访问 URL 段 + metadata 数据源），identifier 规范。
        // 空串放行——内部发布链 ensure 构造无 user 上下文（回填已存值或空，
        // record 侧空转 None）；外部 REST 路径的必填由 handler 层校验兜底。
        if !request.user_id.trim().is_empty()
            && !shared_types::IDENTIFIER_RE.is_match(request.user_id.trim())
        {
            return Err(AppOperationError::Validation(
                "user_id 仅允许字母、数字、下划线和连字符（1-64 字符）".to_string(),
            ));
        }
        // app_id：外部指定（app- + DNS-1123，校验 + 唯一性）or 自动生成
        let app_id = match &request.app_id {
            Some(id) => {
                validate_app_id(id)?;
                // 唯一性：已存在 → ERR_APP_ALREADY_EXISTS（防止 SSA force=true 静默覆盖）
                match self.runtime.get_deployment_status(id).await {
                    Ok(Some(_)) => {
                        return Err(AppOperationError::AlreadyExists(format!(
                            "app already exists: {id}"
                        )));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(map_runtime_error(
                            &format!("[APP] check app existence failed app_id={id}"),
                            error,
                        ));
                    }
                }
                id.clone()
            }
            None => format!("app-{}", &Uuid::new_v4().to_string()[..8]),
        };

        // 资源限制格式（K8s Quantity: storage / ephemeral_storage）→ ERR_VALIDATION
        if let Some(ref resources) = request.resources {
            if let Some(ref s) = resources.storage {
                validate_k8s_storage_size(s).map_err(|e| {
                    AppOperationError::Validation(format!("invalid storage '{}': {}", s, e))
                })?;
            }
            if let Some(ref es) = resources.ephemeral_storage {
                validate_k8s_storage_size(es).map_err(|e| {
                    AppOperationError::Validation(format!(
                        "invalid ephemeral_storage '{}': {}",
                        es, e
                    ))
                })?;
            }
        }

        // 端口校验：HTTP 端口数上限放开（app-runtime 镜像单容器带 pgweb 8081 + ttyd 7681 + 用户应用端口）
        // Pingora 免端口路由 /proxy/userapp/prod 按 (app_id, APP_ENTRY_PORT) 优先（多 HTTP 端口仍全量注册）
        // gateway 模式（HTTPRoute）仍只支持单 HTTP，在 k8s_deployment 侧单独拦截（这里不拦，让 Pingora 模式可用）
        let http_port_count = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.expose_type == ExposeType::Http)
                    .count()
            })
            .unwrap_or(0);
        const MAX_HTTP_PORTS: usize = 8;
        if http_port_count > MAX_HTTP_PORTS {
            return Err(AppOperationError::Validation(format!(
                "at most {MAX_HTTP_PORTS} HTTP ports allowed (got {http_port_count})"
            )));
        }
        // 端口号唯一：避免 K8s annotation 解码歧义（同 port 不同 type 会被 HashMap 折叠）
        // 及 Pingora backend key(port) 冲突。Fail Fast 在源头拒绝。
        if let Some(ports) = &request.ports {
            let mut seen = std::collections::HashSet::new();
            for p in ports {
                if !seen.insert(p.port) {
                    return Err(AppOperationError::Validation(format!(
                        "port {} duplicate: each port number must be unique",
                        p.port
                    )));
                }
                // Service 端口保留名：ttyd/pgweb 由平台恒补（运行容器终端/PG
                // 控制台的代理上游），用户占用会挤掉恒补暴露（K8s 端口名唯一）
                if p.name == "ttyd" || p.name == "pgweb" {
                    return Err(AppOperationError::Validation(format!(
                        "port name '{}' is reserved for platform builtin services (ttyd=7681, pgweb=8081)",
                        p.name
                    )));
                }
            }
        }
        Ok(app_id)
    }

    /// provision：ensure per-app PVC（带用户配额 requests.storage）。
    ///
    /// 顺序硬约束：K8s ensure PVC 必须在 create_deployment 之前——首次 ensure
    /// 带配额，否则 create_deployment 内 ensure 命中 active 复用会丢配额。
    /// 工作空间目录（code/data/logs）不再由 rcoder 建：K8s 镜像自带 + app-cli
    /// 部署段换 code/；Docker bind 源目录由 docker runtime 在 create 前建。
    async fn provision_app_workspace(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<()> {
        let storage_size = request
            .resources
            .as_ref()
            .and_then(|r| r.storage.as_deref());
        self.ensure_app_workspace_ready(app_id, storage_size).await
    }

    /// 创建运行时资源：build params → create_deployment → 注册 Pingora backend。
    ///
    /// 注: UserApp 是新开发逻辑 (application-management-service-v2-design.md), /app 路径
    /// 不涉及历史数据迁移 → 不调 lazy_migrate (新应用无旧数据)。Web/Computer 有历史数据才调。
    async fn create_app_runtime(&self, app_id: &str, request: &CreateAppRequest) -> AppResult<()> {
        let params = self.build_container_params(app_id, request).await?;
        let container_info = self.runtime.create_deployment(params).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] create_deployment failed app_id={app_id}"),
                e,
            )
        })?;
        info!(
            "[APP] app resources created: {} (container={})",
            app_id, container_info.container_name
        );
        // Docker 模式：为 HTTP 端口注册 Pingora backend（/proxy/userapp/prod 免端口代理按 APP_ENTRY_PORT 优查 → container_ip）
        // 注册错误忽略不阻断：pingora backend 幂等可重建（update/restart/启动时 rebuild 会补齐），
        // 不应因注册失败回滚已建成的 Deployment。
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(app_id, &http_ports, &container_info.container_ip)
            .await;
        Ok(())
    }

    /// 装配 AppInfo：实时查运行时状态，合并端口 external_port（K8s node_port），构建 access/health/status。
    ///
    /// status 用运行时 phase 映射（不再硬编码 Running）——刚创建的 Pod 通常还是 Starting，甚至镜像
    /// 拉取失败已 Error；返回真实状态避免"status=Running 但 health=Starting/Error"自相矛盾。
    async fn assemble_app_info(&self, app_id: String, request: CreateAppRequest) -> AppInfo {
        let runtime_status = self.fetch_runtime_status(&app_id).await;

        // 端口状态：以请求端口为准（expose_type 语义完整），合并运行时返回的 external_port（K8s node_port）。
        // Docker 模式 get_deployment_status 不还原端口语义，Tcp 的 host_port 留空（已知限制）。
        let mut ports: Vec<AppPortStatus> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortStatus {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        external_port: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(status) = &runtime_status {
            for rt_p in &status.ports {
                let Some(ep) = rt_p.external_port else {
                    continue;
                };
                // 按 port 匹配 external_port（Docker get_deployment_status 的 name 是
                // tcp-{port}，与请求 name 不一致；port 唯一，K8s/Docker 通用）
                if let Some(ap) = ports.iter_mut().find(|p| p.port == rt_p.port) {
                    ap.external_port = Some(ep);
                }
            }
        }

        let access = self.build_access_info(&app_id, &ports);
        let health = runtime_status
            .as_ref()
            .map(health_from_status)
            .unwrap_or(HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            });
        let (status, message) = match &runtime_status {
            Some(s) => (phase_to_status(&s.phase), s.message.clone()),
            None => (AppStatus::Starting, None),
        };

        let now = Utc::now().to_rfc3339();
        AppInfo {
            app_id,
            name: request.name,
            status,
            message,
            image: request.image.clone().unwrap_or_default(),
            command: request.command.unwrap_or_default(),
            replicas: 1,
            access,
            health,
            resources: request.resources,
            env: request.env.unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}
