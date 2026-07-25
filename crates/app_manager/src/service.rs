//! 应用管理服务层（统一 Docker / K8s 后端，无状态）
//!
//! rcoder 是无状态的应用 pod 引擎：
//! - 写操作（create/start/stop/restart/delete）转调 [`ContainerRuntime`] 的 Deployment 能力；
//! - 读操作（get/query/list）实时查集群，返回 [`AppRuntimeInfo`]；
//! - 业务元数据（name/image/command/env 等）由调用方（Java）持久化，rcoder 不存。
//!
//! K8s 模式 `create_deployment` 创建 ConfigMap/Secret/ClusterIP Service/Deployment；
//! HTTP 入口按 `http_expose`：Pingora（默认，两后端统一，本服务注册 Pingora backend
//! `/proxy/apps/{app_id}/{port}` → 后端 host：Docker container_ip / K8s ClusterIP FQDN）或 Gateway
//! （可选，K8s 建 HTTPRoute `/apps/{id}`）。TCP 初期不对外。Docker 模式建容器入主网络。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;
use tokio::fs;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{
    AppHealthCheck, AppPortSpec, AppResourceRequirements, ContainerCreateParams, ContainerRuntime,
    DeploymentStatus, ExposeType as RtExposeType, HttpExpose,
};
use rcoder_proxy::PingoraProxyService;
use shared_types::ServiceType;

use super::config::{AppAccessMode, AppManagerConfig};
use super::models::*;
use super::utils::*;

/// 应用管理服务（Docker / K8s 统一）
pub struct AppService {
    pub(crate) config: AppManagerConfig,
    pub(crate) runtime: Arc<dyn ContainerRuntime>,
    /// Pingora 代理（Docker 模式用于注册 HTTP backend；K8s 模式通常为 None）
    pub(crate) pingora: Option<Arc<PingoraProxyService>>,
    /// 路径解析器缓存（单例；Docker 模式将 rcoder 容器内路径解析为宿主机路径）
    pub(crate) path_resolver: Cache<String, Arc<HostPathResolver>>,
    /// Docker 模式 Pingora backend 端口登记（app_id → 注册的 HTTP 端口列表）
    ///
    /// 这是**操作副作用的临时缓存**（非业务元数据）：delete 时需要知道曾注册过哪些端口
    /// 才能清理 Pingora backend。rcoder 重启后丢失可接受（Docker 模式定位为开发环境）。
    pub(crate) pingora_ports: DashMap<String, Vec<u16>>,
}

impl AppService {
    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn ContainerRuntime>,
        pingora: Option<Arc<PingoraProxyService>>,
    ) -> AppResult<Self> {
        let path_resolver: Cache<String, Arc<HostPathResolver>> =
            Cache::builder().max_capacity(1).build();

        // 初始化路径解析器（失败不致命，Docker 模式回退到容器内路径）
        match HostPathResolver::new().await {
            Ok(resolver) => {
                info!("[APP] path resolver initialized");
                path_resolver.insert("default".to_string(), Arc::new(resolver));
            }
            Err(e) => {
                warn!("[APP] path resolver init failed, using container path: {}", e);
            }
        }

        // K8s 模式：启动校验前置条件（RBAC 等）。失败 log warn 不阻塞（Fail Fast 暴露部署侧
        // RBAC 缺失，而非运行时创建 app 才 403）。Docker 模式 trait 默认 Ok，跳过。
        if config.access_mode == AppAccessMode::Kubernetes {
            match runtime.validate_app_prerequisites().await {
                Ok(_) => info!("[APP] K8s prerequisites validated (RBAC/apps/deployments accessible)"),
                Err(e) => warn!("[APP] K8s prerequisites validation failed, app management may not work: {}", e),
            }
        }

        // 无效组合告警（Fail Fast）：Docker 无 HTTPRoute/gateway 概念，gateway 模式不可用。
        // 不阻塞启动（便于临时切回 pingora），但 HTTP 将不可访问。
        if config.access_mode == AppAccessMode::Docker && config.http_expose == HttpExpose::Gateway
        {
            warn!(
                "[APP] invalid combo access_mode=docker + http_expose=gateway: Docker has no HTTPRoute, gateway mode unavailable, HTTP will be inaccessible; set RCODER_APP_HTTP_EXPOSE=pingora"
            );
        }

        let svc = Self {
            config,
            runtime,
            pingora,
            path_resolver,
            pingora_ports: DashMap::new(),
        };
        // K8s Pingora 模式：启动时从集群重建 Pingora backends——修复 pingora_ports 内存态
        // 丢失导致的重启 silent 404（list_deployments 的 expose_type 已由 Deployment annotation
        // 准确还原）。失败不阻塞启动（warn，待下次 create/update 恢复）。
        if svc.config.access_mode == AppAccessMode::Kubernetes
            && svc.config.http_expose == HttpExpose::Pingora
            && let Err(e) = svc.rebuild_pingora_backends().await
        {
            warn!(
                "[APP] pingora backends rebuild failed (HTTP temporarily unreachable after restart, recovered on next create/update): {}",
                e
            );
        }
        Ok(svc)
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        // app_id：外部指定（app- + DNS-1123，校验 + 唯一性）or 自动生成
        let app_id = match &request.app_id {
            Some(id) => {
                validate_app_id(id)?;
                // 唯一性：已存在 → ERR_APP_ALREADY_EXISTS（防止 SSA force=true 静默覆盖）
                if let Ok(Some(_)) = self.runtime.get_deployment_status(id).await {
                    return Err(AppOperationError::AlreadyExists(format!(
                        "app already exists: {id}"
                    )));
                }
                id.clone()
            }
            None => format!("app-{}", &Uuid::new_v4().to_string()[..8]),
        };
        info!(
            "[APP] creating app: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );

        // 0. 校验资源限制格式（K8s Quantity: storage / ephemeral_storage）→ ERR_VALIDATION
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

        // 0.5 校验端口：HTTP 端口数上限放开（app-runtime 镜像单容器带 pgweb 8081 + ttyd 7681 + 用户应用端口）
        // Pingora path 路由 /proxy/apps/{app_id}/{port}/ 按 (app_id, port) 区分，天然支持多 HTTP 端口
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
        // 0.5b 端口号唯一：避免 K8s annotation 解码歧义（同 port 不同 type 会被 HashMap 折叠）
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
            }
        }

        // 1. K8s: ensure per-app PVC 带用户配额 requests.storage + 等 subvolumePath 就绪。Docker no-op。
        //    必须在 create_app_dirs (建目录) + create_deployment (Docker bind mount 需源目录存在) 之前:
        //    首次 ensure 带配额, 避免 create_deployment 内 ensure 命中 active 复用丢配额。
        let storage_size = request.resources.as_ref().and_then(|r| r.storage.as_deref());
        self.ensure_app_workspace_ready(&app_id, storage_size).await?;

        // 2. 创建应用工作空间目录（code/data/logs）—— Docker: 共享 Local (create_deployment bind mount 源,
        //    必须先存在); K8s: per-app PVC 根 (ensure_app_workspace_ready 已 ensure + 等 subvolumePath)。
        self.create_app_dirs(&app_id).await?;

        // 3. 构建容器创建参数（UserApp）
        let params = self.build_container_params(&app_id, &request).await?;

        // 4. 创建 Deployment / 容器（K8s 含 ConfigMap/Secret/Service/HTTPRoute/NodePort;
        //    PVC active 复用 / Docker bind mount 共享目录已存在）。
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

        // 注: UserApp 是新开发逻辑 (application-management-service-v2-design.md), /app 路径
        // 不涉及历史数据迁移 → 不调 lazy_migrate (新应用无旧数据)。
        // Web/Computer 有历史数据 → 保留 lazy_migrate。

        // 5. Docker 模式：为 HTTP 端口注册 Pingora backend（/proxy/apps/{app_id}/{port} → container_ip）
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(&app_id, &http_ports, &container_info.container_ip)
            .await;

        // 6. 实时查询运行时状态（K8s 用于拿真实 node_port；Docker 模式不还原端口语义）
        let runtime_status = self.fetch_runtime_status(&app_id).await;

        // 端口状态：以请求端口为准（expose_type 语义完整），合并运行时返回的 external_port
        // （K8s node_port）。Docker 模式 get_deployment_status 不还原端口语义，Tcp 的 host_port
        // 留空（已知限制：Docker Tcp 对外端口需通过 docker inspect port_bindings 另查）。
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

        // 7. 构建访问信息 + 健康信息
        let access = self.build_access_info(&app_id, &ports);
        let health = runtime_status
            .as_ref()
            .map(health_from_status)
            .unwrap_or(HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            });

        // status：用刚查到的运行时 phase 映射（不再硬编码 Running）——刚创建的 Pod 通常
        // 还是 Starting，甚至镜像拉取失败已 Error；返回真实状态避免"status=Running 但
        // health=Starting/Error"自相矛盾。message 带 phase=Error 的失败原因。
        let (status, message) = match &runtime_status {
            Some(s) => (phase_to_status(&s.phase), s.message.clone()),
            None => (AppStatus::Starting, None),
        };

        let now = Utc::now().to_rfc3339();
        Ok(AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status,
            message,
            image: request.image.clone(),
            command: request.command.clone().unwrap_or_default(),
            replicas: 1,
            access,
            health,
            resources: request.resources.clone(),
            env: request.env.clone().unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
    #[instrument(skip(self))]
    pub async fn list_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?;
        Ok(statuses
            .into_iter()
            .map(|s| self.build_runtime_info(s))
            .collect())
    }

    /// 查询应用列表（实时查集群 + 过滤/分页）
    #[instrument(skip(self, request))]
    pub async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>> {
        let mut items = self.list_app_runtimes().await?;

        // 过滤（仅 status/app_ids 为运行时字段，可生效；name/created_at 需业务元数据，跳过）
        if let Some(filters) = &request.filters {
            if let Some(status) = &filters.status {
                items.retain(|app| status.contains(&app.status));
            }
            if let Some(app_ids) = &filters.app_ids {
                items.retain(|app| app_ids.contains(&app.app_id));
            }
            if filters.name.is_some() || filters.created_at.is_some() {
                warn!(
                    "[APP] query_apps name/created_at filters require business metadata, rcoder is stateless, ignored"
                );
            }
        }

        // 排序（仅 app_id 可用；默认升序，Desc 时降序）
        if let Some(sort_by) = &request.sort_by
            && (sort_by == "app_id" || sort_by == "name")
        {
            items.sort_by(|a, b| a.app_id.cmp(&b.app_id));
            if request.sort_order == Some(SortOrder::Desc) {
                items.reverse();
            }
        }

        // 分页
        let total = items.len() as u64;
        let page = request.page.unwrap_or(1).max(1);
        let page_size = request.page_size.unwrap_or(20).min(100);
        let start = ((page - 1) * page_size) as usize;
        let end = (start + page_size as usize).min(items.len());
        let paged_items = if start < items.len() {
            items[start..end].to_vec()
        } else {
            vec![]
        };

        Ok(PaginatedResponse {
            items: paged_items,
            pagination: Pagination {
                page,
                page_size,
                total,
                total_pages: ((total as f64) / (page_size as f64)).ceil() as u32,
            },
        })
    }

    /// 获取应用运行时详情（实时查集群；精确区分 404 与 500）
    #[instrument(skip(self))]
    pub async fn get_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        Ok(self.build_runtime_info(status))
    }

    /// 更新应用配置
    /// 更新应用（v2 §5.2，全量替换 desired state）。
    ///
    /// rcoder 无状态：不持有旧 desired state，故本操作为**全量替换**——调用方需发送完整
    /// 新状态（`image` 必填）。K8s 走 SSA re-apply（幂等）+ orphan 端口/配置清理；
    /// Docker 重建容器（image/env/command 变化必须重建），工作空间目录保留。
    #[instrument(skip(self, request))]
    pub async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        let current = self.fetch_runtime_status_or_err(app_id).await?;
        // 乐观锁：expected_resource_version 不匹配 → 409 Conflict
        // （Docker resource_version=None → 跳过校验，开发环境 last-write-wins 可接受）
        if let Some(expected) = &request.expected_resource_version
            && let Some(actual) = &current.resource_version
            && expected != actual
        {
            return Err(AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            )));
        }
        let params = self.build_container_params_from_update(app_id, &request).await?;
        // 先注销旧 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 下方按本次 http_ports 重新注册到 Service FQDN，注销-重注成对保证一致）。
        self.unregister_pingora_backends(app_id).await;
        let info = self.runtime.patch_deployment(params).await.map_err(|e| {
            map_runtime_error(&format!("[APP] patch_deployment failed app_id={app_id}"), e)
        })?;
        // 重新注册 Pingora backend。http_ports 取本次请求 ports；若未带（K8s 下 update 常
        // 只改 image/env 等部分字段），沿用当前 Deployment 的 HTTP 端口，保证与上面
        // unregister 对称——否则部分更新会丢 Pingora 路由（app 经 /proxy/apps/{id}/{port} 变 502）。
        // 注：register 在 K8s 模式并非 no-op，会把 backend 指到 Service FQDN（与 create 一致）。
        let http_ports = if request.ports.is_some() {
            http_port_numbers(&request.ports)
        } else {
            current
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect::<Vec<u16>>()
        };
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] app updated: {}", app_id);
        self.get_app(app_id).await
    }

    /// 删除应用（v2 §5.3：默认保留持久存储，purge=true 才清空数据面）。
    #[instrument(skip(self))]
    pub async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        // 乐观锁（同 update_app）：expected 不匹配 → 409 Conflict
        if let Some(expected) = expected_resource_version {
            let current = self.fetch_runtime_status_or_err(app_id).await?;
            if let Some(actual) = &current.resource_version
                && expected != actual
            {
                return Err(AppOperationError::Conflict(format!(
                    "resource version mismatch: expected={expected}, actual={actual}"
                )));
            }
        }
        info!("[APP] deleting app: {} (purge={})", app_id, purge);

        // 1. Docker 模式：清理 Pingora backend
        self.unregister_pingora_backends(app_id).await;

        // 2. 删除计算资源（K8s: Deployment/Service/HTTPRoute/NodePort/ConfigMap/Secret
        //    + label orphan 扫描兜底；Docker: 容器）。持久存储默认保留。
        self.runtime.delete_deployment(app_id).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] delete_deployment failed app_id={app_id}"),
                e,
            )
        })?;

        // 3. 仅 purge=true 时清空持久存储（code/data/logs 目录）。
        //    默认保留：应用可重建，数据不可再生（v2 §5.3 数据安全）。
        if purge {
            let app_dir = self.get_container_app_dir(app_id).await?;
            // K8s per-agent: app_dir = per-app PVC 根, 清空内容不删根 (同 delete_app_storage)
            if app_dir.exists()
                && let Err(e) = Self::purge_dir_contents(&app_dir).await
            {
                warn!("[APP] purge dir contents failed {:?}: {}", app_dir, e);
            }
            info!("[APP] persistent storage cleared: {}", app_id);
        } else {
            info!(
                "[APP] retained persistent storage (pass purge=true to clear): {}",
                app_id
            );
        }

        Ok(())
    }

    /// 重置 app 容器内 PG 密码(rcoder exec 容器内 psql ALTER USER,本地 trust 认证绕过当前密码)。
    /// 解决"用户忘记密码进不去 pgweb"的死锁(pgweb 要当前密码,rcoder 用容器内 trust 免密)。
    pub async fn reset_db_password(
        &self,
        app_id: &str,
        req: ResetDbPasswordRequest,
    ) -> AppResult<()> {
        self.ensure_app_running(app_id).await?;
        if req.new_password.is_empty() {
            return Err(AppOperationError::Validation(
                "new_password must not be empty".to_string(),
            ));
        }
        // 容器内 sh 展开 $POSTGRES_USER(镜像 ENV,create 时用户 env 覆盖);rcoder 无状态不知值。
        // psql -U $POSTGRES_USER 本地 trust 认证(start-app.sh initdb --auth-local=trust)免密。
        // SQL 字符串里 ' 转义为 ''(防注入)。ON_ERROR_STOP=1:出错 exit≠0。
        let safe_pw = req.new_password.replace('\'', "''");
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c "ALTER USER \"$POSTGRES_USER\" WITH PASSWORD '{safe_pw}'""#,
            ),
        ];
        self.exec_psql(
            app_id,
            cmd,
            &format!("[APP] reset_db_password failed app_id={app_id}"),
        )
        .await?;
        info!("[APP] PG password reset: {}", app_id);
        Ok(())
    }

    /// 新建 PG 库(rcoder exec 容器内 psql CREATE DATABASE)。API 化建库(Java/CI 自动化)。
    pub async fn create_database(
        &self,
        app_id: &str,
        req: CreateDatabaseRequest,
    ) -> AppResult<()> {
        self.ensure_app_running(app_id).await?;
        validate_pg_identifier(&req.database)?;
        if let Some(owner) = &req.owner {
            validate_pg_identifier(owner)?;
        }
        // 先查是否已存在(check-then-act): PG 不支持 CREATE DATABASE IF NOT EXISTS、也不能进事务/DO 块。
        // 故先 SELECT pg_database 判定, 避免 CREATE 失败后靠 stderr 文本(随 PG 版本/locale 变)判"已存在"。
        if self.database_exists(app_id, &req.database).await? {
            return Err(AppOperationError::AlreadyExists(format!(
                "database {} already exists",
                req.database
            )));
        }
        // CREATE DATABASE "{db}"[ OWNER "{owner}"] —— 双引号 PG 标识符," 转义为 ""
        let safe_db = req.database.replace('"', "\"\"");
        let owner_clause = req
            .owner
            .as_ref()
            .map(|o| format!(" OWNER \"{}\"", o.replace('"', "\"\"")))
            .unwrap_or_default();
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -v ON_ERROR_STOP=1 -c 'CREATE DATABASE "{safe_db}"{owner_clause}'"#,
            ),
        ];
        let ctx = format!("[APP] create_database failed app_id={app_id}");
        let r = self
            .runtime
            .exec(app_id, cmd)
            .await
            .map_err(|e| map_runtime_error(&ctx, e))?;
        if r.exit_code != 0 {
            // 罕见竞态: SELECT 时不存在、CREATE 时被并发创建 → 再查一次精确判定, 仍不靠 stderr 文本。
            if self.database_exists(app_id, &req.database).await? {
                return Err(AppOperationError::AlreadyExists(format!(
                    "database {} already exists",
                    req.database
                )));
            }
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        info!(
            "[APP] database created: {} (app_id={})",
            req.database, app_id
        );
        Ok(())
    }

    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 1)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        info!("[APP] app started (scale=1): {}", app_id);
        self.get_app(app_id).await
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 0)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        info!("[APP] app stopped (scale=0): {}", app_id);
        self.get_app(app_id).await
    }

    /// 重启应用（rollout restart）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.restart_deployment(app_id).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] restart_deployment failed app_id={app_id}"),
                e,
            )
        })?;
        info!("[APP] app restarted (rollout): {}", app_id);
        self.get_app(app_id).await
    }

    /// 获取应用日志（实时拉容器 stdout/stderr：K8s Pod logs / docker logs）。
    ///
    /// `follow` 流式当前未实现（runtime 返回 tail 快照），`since` 暂未透传；
    /// SSE/WebSocket 实时流留待后续增强。
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        let tail = params.tail.unwrap_or(1000);
        let timestamps = params.timestamps.unwrap_or(true);
        let entries = self
            .runtime
            .get_app_logs(app_id, tail, timestamps)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] get_app_logs failed app_id={app_id}"), e)
            })?;
        Ok(entries
            .into_iter()
            .map(|e| LogEntry {
                timestamp: e.timestamp.unwrap_or_default(),
                stream: e.stream,
                message: e.message,
            })
            .collect())
    }

    /// 启动日志流（follow），返回 mpsc::Receiver 供 WS handler 桥接（v2 §11）。
    /// receiver drop 即取消：客户端断开 → handler 退出 → receiver 析构 → runtime 任务终止。
    pub async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> AppResult<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>
    {
        validate_app_id(app_id)?;
        self.runtime
            .stream_app_logs(app_id, tail)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] stream_app_logs failed app_id={app_id}"), e)
            })
    }

    /// 获取资源使用情况。
    ///
    /// CPU/内存用量 + 限额来自运行时（K8s = metrics.k8s.io PodMetrics + pod limits；Docker 默认 0），
    /// 百分比 = usage/limit×100（limit=0 → 0）。restart_count 来自 Deployment 状态。
    /// network（rx/tx）metrics.k8s.io 不提供，留 0。运行时用量查询失败降级为 0（不 500）。
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        validate_app_id(app_id)?;
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        let usage = match self.runtime.get_app_resource_usage(app_id).await {
            Ok(u) => u,
            Err(e) => {
                warn!(
                    "[APP] get_app_resource_usage failed app_id={app_id}: {e} (stats 降级 0)"
                );
                Default::default()
            }
        };
        let cpu_percent = if usage.cpu_limit_cores > 0.0 {
            (usage.cpu_usage_cores / usage.cpu_limit_cores * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let mem_percent = if usage.mem_limit_bytes > 0 {
            usage.mem_usage_bytes as f64 / usage.mem_limit_bytes as f64 * 100.0
        } else {
            0.0
        };
        Ok(ResourceStats {
            restart_count: status.restart_count,
            cpu: CpuStats {
                usage_cores: usage.cpu_usage_cores,
                limit_cores: usage.cpu_limit_cores,
                usage_percent: cpu_percent,
            },
            memory: MemoryStats {
                usage_bytes: usage.mem_usage_bytes,
                limit_bytes: usage.mem_limit_bytes,
                usage_percent: mem_percent,
            },
            network: NetworkStats::default(),
        })
    }

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃）
    #[instrument(skip(self))]
    pub async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.get_app_events(app_id).await.map_err(|e| {
            map_runtime_error(&format!("[APP] get_app_events failed app_id={app_id}"), e)
        })
    }

    /// 读取应用文件日志（从 workspace PVC 的 logs/ 目录直接读，不依赖 K8s Pod log API）。
    ///
    /// 适用：不写 stdout 而写文件的应用（Java Spring Boot → logs/application.log 等）。
    /// 路径相对 app 根（如 "logs/app.log"），有 path traversal 防护。
    #[instrument(skip(self))]
    pub async fn get_app_file_logs(
        &self,
        app_id: &str,
        file_path: &str,
        tail: u32,
    ) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let target = app_dir.join(file_path);

        // exists 守卫：日志文件不存在返 FileNotFound（常见，非 500）；canonicalize 失败也归此类
        if !target.exists() {
            return Err(AppOperationError::FileNotFound(format!(
                "log file does not exist: {file_path}"
            )));
        }
        // path traversal 防护（与 upload/delete_file 一致，复用 utils::ensure_within_app_dir）
        let canonical_root = app_dir
            .canonicalize()
            .unwrap_or_else(|_| app_dir.clone());
        let canonical_target = ensure_within_app_dir(&target, &canonical_root)?;

        // 读文件，取最后 tail 行
        let content = tokio::fs::read_to_string(&canonical_target)
            .await
            .map_err(|e| {
                map_io_error(&format!("failed to read log file '{file_path}'"), e, true)
            })?;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail as usize);
        Ok(lines[start..]
            .iter()
            .map(|line| LogEntry {
                timestamp: String::new(),
                stream: "file".to_string(),
                message: line.to_string(),
            })
            .collect())
    }

    // ========================================================================
    // 辅助方法
    // ========================================================================

    /// 构建 ContainerCreateParams（UserApp，create 路径）
    async fn build_container_params(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<ContainerCreateParams> {
        self.build_params_inner(
            app_id,
            request.image.clone(),
            &request.command,
            &request.env,
            &request.secrets,
            &request.ports,
            &request.health_check,
            &request.resources,
            &request.tenant_id,
            &request.space_id,
        )
        .await
    }

    /// UpdateAppRequest → ContainerCreateParams（全量替换语义，image 必填）。
    /// image 缺失 → ERR_VALIDATION（rcoder 无状态，无法保留旧 image，调用方必须发完整新状态）。
    async fn build_container_params_from_update(
        &self,
        app_id: &str,
        request: &UpdateAppRequest,
    ) -> AppResult<ContainerCreateParams> {
        let image = request.image.clone().ok_or_else(|| {
            AppOperationError::Validation(
                "update requires image (rcoder is stateless, cannot retain previous image)"
                    .to_string(),
            )
        })?;
        self.build_params_inner(
            app_id,
            image,
            &request.command,
            &request.env,
            &request.secrets,
            &request.ports,
            &request.health_check,
            &request.resources,
            &request.tenant_id,
            &request.space_id,
        )
        .await
    }

    /// build_container_params / build_container_params_from_update 的公共逻辑。
    ///
    /// 参数全用引用（`&Option<...>`），内部按需 `.clone()` 取值；`image` 为 owned `String`
    /// （create 直接传 `request.image.clone()`；update 先 ok_or 校验再传入）。
    /// 统一 create/update 两路逻辑：此前重复 ~180 行（90% 相同），分歧仅在 image 校验。
    #[allow(clippy::too_many_arguments)]
    async fn build_params_inner(
        &self,
        app_id: &str,
        image: String,
        command: &Option<Vec<String>>,
        env: &Option<HashMap<String, String>>,
        secrets: &Option<HashMap<String, String>>,
        ports: &Option<Vec<PortConfig>>,
        health_check: &Option<HealthCheckConfig>,
        resources: &Option<ResourceLimits>,
        tenant_id: &Option<String>,
        space_id: &Option<String>,
    ) -> AppResult<ContainerCreateParams> {
        // 端口：models::PortConfig → container_runtime_api::AppPortSpec
        let app_ports: Vec<AppPortSpec> = ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortSpec {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        strip_prefix: p.strip_prefix,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Exec 健康检查当前未支持（AppHealthCheck 无 command 字段），Fail Fast 拒绝，
        // 避免静默丢弃用户配置（K8s build_probe 对 Exec 返回 None → 容器被视为永远健康）
        if let Some(hc) = health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            return Err(AppOperationError::Validation(
                "Exec health check is not supported (AppHealthCheck lacks command field); use Http/Tcp instead"
                    .to_string(),
            ));
        }

        // 健康检查：models::HealthCheckConfig → AppHealthCheck
        let app_health_check = health_check.as_ref().map(|hc| AppHealthCheck {
            check_type: map_health_check_type(&hc.check_type),
            path: hc.path.clone(),
            port: hc.port,
            initial_delay_seconds: None,
            period_seconds: None,
        });

        // 资源：models::ResourceLimits → AppResourceRequirements
        let app_resources = resources.as_ref().map(|r| AppResourceRequirements {
            cpu: r.cpu.clone(),
            memory: r.memory.clone(),
            storage: r.storage.clone(),
            ephemeral_storage: r.ephemeral_storage.clone(),
        });

        // 宿主机工作空间路径（Docker 模式 bind mount 源；K8s 模式 runtime 用 subPath，忽略此值）
        let host_workspace_path = self
            .get_host_app_dir(app_id)
            .await
            .to_string_lossy()
            .to_string();

        let mut builder = ContainerCreateParams::builder()
            .project_id(app_id.to_string())
            .service_type(ServiceType::UserApp)
            .host_workspace_path(host_workspace_path)
            .image_override(image)
            .env(env.clone().unwrap_or_default())
            .secrets(secrets.clone().unwrap_or_default())
            .ports(app_ports);

        // command 仅在非空时设置（空 vec 会覆盖镜像 CMD）
        if let Some(cmd) = command.clone()
            && !cmd.is_empty()
        {
            builder = builder.command(cmd);
        }
        if let Some(hc) = app_health_check {
            builder = builder.health_check(hc);
        }
        if let Some(ar) = app_resources {
            // 阶段2: storage 落 per-app PVC requests.storage (CSI 服务端 subvolume 配额);
            // ephemeral_storage 仍限 overlay 可写层。
            if let Some(ss) = ar.storage.clone() {
                builder = builder.storage_size(ss);
            }
            builder = builder.app_resources(ar);
        }
        // tenant/space：进 ContainerCreateParams → build_app_labels 打 rcoder.io/tenant、
        // rcoder.io/space label（供对账/过滤）。
        if let Some(t) = tenant_id.clone() {
            builder = builder.tenant_id(t);
        }
        if let Some(s) = space_id.clone() {
            builder = builder.space_id(s);
        }

        Ok(builder.build())
    }

    /// 实时查询单个应用运行时状态（None 表示不存在）
    async fn fetch_runtime_status(&self, app_id: &str) -> Option<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("[APP] query runtime status failed app_id={}: {}", app_id, e);
                None
            }
        }
    }

    /// 实时查状态，精确区分两种"查不到"：Ok(None)=集群中真不存在 → "应用不存在"(→404)；
    /// Err=API Server 不可达/RBAC 拒绝 → "查询应用状态失败"(→500)。
    ///
    /// 供需要精确错误分类的读路径（get_app/get_app_stats/ensure_app_exists）使用，
    /// 替代会塌缩错误的 `fetch_runtime_status`（后者仅供 create_app 这类 None 可接受的场景）。
    /// 若误用 fetch_runtime_status，瞬时 API 错误会被当成"应用不存在"→404，触发 Java 误重建。
    async fn fetch_runtime_status_or_err(&self, app_id: &str) -> AppResult<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(AppOperationError::NotFound(format!(
                "app does not exist: {app_id}"
            ))),
            Err(e) => {
                warn!("[APP] query app status failed app_id={}: {}", app_id, e);
                Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )))
            }
        }
    }

    /// 确认 app 存在（集群中有 Deployment/容器），不存在返回"应用不存在"错误。
    /// 调用方（start/stop/restart）据此返回 404，方便 Java 区分并触发 create 重建，
    /// 而非收到 generic 500 误以为系统故障。
    async fn ensure_app_exists(&self, app_id: &str) -> AppResult<()> {
        self.fetch_runtime_status_or_err(app_id).await.map(|_| ())
    }

    /// 校验 app 处于 Running 阶段（exec psql 的前置条件）。
    ///
    /// Stopped/Starting 等给 InvalidState 友好错误而非让 exec 失败（exec 在 Stopped 时
    /// 报容器不存在的 Backend 错误，对用户不友好）。reset_db_password / create_database 共用。
    async fn ensure_app_running(&self, app_id: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        if status.phase != "Running" {
            return Err(AppOperationError::InvalidState(format!(
                "app {app_id} not running (phase={}), exec psql requires a live container",
                status.phase
            )));
        }
        Ok(())
    }

    /// exec 容器内 psql 命令，exit_code != 0 → Backend 错误（含 stderr 摘要）。
    ///
    /// reset_db_password 共用。create_database 因需区分"库已存在"(AlreadyExists) 不复用此函数。
    async fn exec_psql(
        &self,
        app_id: &str,
        command: Vec<String>,
        ctx: &str,
    ) -> AppResult<()> {
        let r = self.runtime.exec(app_id, command).await.map_err(|e| {
            map_runtime_error(ctx, e)
        })?;
        if r.exit_code != 0 {
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        Ok(())
    }

    /// 查询 app 容器 PG 里某库是否已存在（psql `-tAc SELECT pg_database`）。
    /// `-tAc` 取无表头纯输出: 命中输出 `1`、未命中输出空 → 比 CREATE 失败后解析 stderr 稳定。
    /// `db` 已过 `validate_pg_identifier` 白名单(`[a-zA-Z0-9_]`), 安全内联到字符串字面量。
    /// create_database 用此做 check-then-act(替代旧版靠 stderr 文本判"已存在")。
    async fn database_exists(&self, app_id: &str, db: &str) -> AppResult<bool> {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                r#"psql -U "$POSTGRES_USER" -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='{db}'""#,
            ),
        ];
        let ctx = format!("[APP] check database exists failed app_id={app_id}");
        let r = self
            .runtime
            .exec(app_id, cmd)
            .await
            .map_err(|e| map_runtime_error(&ctx, e))?;
        if r.exit_code != 0 {
            return Err(AppOperationError::Backend(format!(
                "{ctx}: exit {}: {}",
                r.exit_code,
                r.stderr.trim()
            )));
        }
        Ok(r.stdout.trim() == "1")
    }

    /// DeploymentStatus → AppRuntimeInfo（含访问地址构建 + conditions 派生）
    fn build_runtime_info(&self, status: DeploymentStatus) -> AppRuntimeInfo {
        let conditions = derive_conditions(&status);

        // Pingora 模式（不论 Docker/K8s）：runtime status 只含 TCP（HTTP 端口无 binding），
        // 从 pingora_ports 补全 HTTP 端口，保证 get 路径的 ports/access 与 create 一致。
        // Gateway 模式：K8s status.ports 已含 HTTP（HTTPRoute backendRef），无需补。
        // ⚠️ 重启风险（pingora_ports 内存态丢失，已知限制）：
        //   - Docker：HTTP 端口补不出 → access.external.http = null（Java 可感知降级）
        //   - K8s Pingora：status.ports（containerPort）仍含 HTTP → access 返有效 /proxy/apps/{app_id}/{port}，
        //     但 Pingora backend 未重注册 → 访问 404（静默坏路径）。根治：启动从 containerPorts 重建 backends（TODO）
        let ports = if self.config.http_expose == HttpExpose::Pingora {
            let mut merged = status.ports.clone();
            if let Some(http_list) = self.pingora_ports.get(&status.app_id) {
                let http_ports: Vec<u16> = http_list.value().clone();
                // drop Ref guard，避免后续借用 self 时持有 DashMap 读锁
                drop(http_list);
                for hp in http_ports {
                    if !merged.iter().any(|p| p.port == hp) {
                        merged.push(AppPortStatus {
                            name: format!("http-{hp}"),
                            port: hp,
                            expose_type: RtExposeType::Http,
                            external_port: None,
                        });
                    }
                }
            }
            merged
        } else {
            status.ports
        };

        let access = self.build_access_info(&status.app_id, &ports);
        AppRuntimeInfo {
            status: phase_to_status(&status.phase),
            access,
            app_id: status.app_id,
            phase: status.phase,
            message: status.message,
            replicas: status.replicas,
            ready_replicas: status.ready_replicas,
            restart_count: status.restart_count,
            pod_ip: status.pod_ip,
            node: status.node,
            started_at: status.started_at,
            ports,
            conditions,
            resource_version: status.resource_version,
        }
    }

    /// 构建访问信息（按 `http_expose` 决定 HTTP path；一律只返 path，host 由 Java 拼）
    fn build_access_info(&self, app_id: &str, ports: &[AppPortStatus]) -> AccessInfo {
        let http_port = ports.iter().find(|p| p.expose_type == RtExposeType::Http);

        // 一律只返 path，host 由 Java 拼（Java 必然已知 RCoder / gateway 入口，否则访问不了）：
        // - Pingora 模式（默认，两后端统一）：/proxy/apps/{app_id}/{port}
        // - Gateway 模式（K8s 可选）：/apps/{app_id}
        // TCP 初期不对外（external.tcp 空）；internal 始终给 ClusterIP FQDN / 容器名。
        let http_url = match self.config.http_expose {
            HttpExpose::Pingora => http_port.map(|p| format!("/proxy/apps/{}/{}", app_id, p.port)),
            HttpExpose::Gateway => http_port.map(|_| format!("/apps/{}", app_id)),
        };

        // internal domain：K8s = ClusterIP Service FQDN；Docker = 容器名（= 资源名）
        let (domain, short_domain) = match self.config.access_mode {
            AppAccessMode::Docker => {
                let name = format!("{}-{}", ServiceType::UserApp.container_prefix(), app_id);
                (name.clone(), name)
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                let svc = format!("{}-{}-svc", ServiceType::UserApp.container_prefix(), app_id);
                (
                    format!("{}.{}.svc.{}", svc, self.config.namespace, cluster_domain),
                    format!("{}.{}", svc, self.config.namespace),
                )
            }
        };

        AccessInfo {
            external: ExternalAccess {
                http: http_url,
                tcp: vec![], // TCP 初期不对外
            },
            internal: InternalAccess {
                domain,
                short_domain,
                ports: ports
                    .iter()
                    .map(|p| InternalPort {
                        name: p.name.clone(),
                        port: p.port,
                    })
                    .collect(),
            },
        }
    }

    /// 为 HTTP 端口注册 Pingora backend（Pingora 模式，Docker/K8s 统一）。
    /// backend host 按后端：Docker=container_ip，K8s=ClusterIP Service FQDN（Pod 内 kube-dns 解析）。
    /// Gateway 模式不注册（HTTP 走 HTTPRoute）。
    async fn register_pingora_backends(
        &self,
        app_id: &str,
        http_ports: &[u16],
        container_ip: &str,
    ) -> Vec<u16> {
        // Gateway 模式 HTTP 走 HTTPRoute，不经 Pingora——跳过
        if self.config.http_expose == HttpExpose::Gateway {
            return vec![];
        }
        let Some(pingora) = &self.pingora else {
            return vec![];
        };
        // backend host：Docker 用 container_ip；K8s 用 ClusterIP Service FQDN（container_ip 为空）
        let backend_host = match self.config.access_mode {
            AppAccessMode::Docker => {
                if container_ip.is_empty() {
                    warn!(
                        "[APP] Docker mode container_ip empty, skip pingora backend registration: {}",
                        app_id
                    );
                    return vec![];
                }
                container_ip.to_string()
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                format!(
                    "{}-{}-svc.{}.svc.{}",
                    ServiceType::UserApp.container_prefix(),
                    app_id,
                    self.config.namespace,
                    cluster_domain
                )
            }
        };
        for port in http_ports {
            pingora.add_app_backend(app_id, *port, backend_host.clone());
        }
        if !http_ports.is_empty() {
            self.pingora_ports
                .insert(app_id.to_string(), http_ports.to_vec());
            info!(
                "[APP] pingora backend registered: {} ports={:?} -> {}",
                app_id, http_ports, backend_host
            );
        }
        http_ports.to_vec()
    }

    /// 清理 app 曾注册的 Pingora backend（Pingora 模式）。Gateway 模式未注册过，直接返回。
    async fn unregister_pingora_backends(&self, app_id: &str) {
        if self.config.http_expose == HttpExpose::Gateway {
            return;
        }
        let Some(pingora) = &self.pingora else {
            return;
        };
        if let Some((_, ports)) = self.pingora_ports.remove(app_id) {
            for port in &ports {
                pingora.remove_app_backend(app_id, *port);
            }
            info!("[APP] pingora backend unregistered: {} ports={:?}", app_id, ports);
        }
    }

    /// 启动时重建 Pingora backends（K8s Pingora 模式，修复重启后 pingora_ports 内存态丢失）。
    /// 从集群列出所有托管 app，按 expose_type（Deployment annotation 还原）重新注册 HTTP 端口的 backend。
    async fn rebuild_pingora_backends(&self) -> AppResult<()> {
        // pingora 未配置（proxy_config 未配）→ 无 backend 可注册；显式说明，避免"0 个 app"被误读为"集群无应用"
        if self.pingora.is_none() {
            info!("[APP] pingora disabled (no proxy_config), skip backends rebuild");
            return Ok(());
        }
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] rebuild list_deployments failed", e))?;
        let mut count = 0;
        for status in &statuses {
            let http_ports: Vec<u16> = status
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect();
            if http_ports.is_empty() {
                continue;
            }
            // register 内部按 access_mode 选 host（K8s=svc FQDN）；container_ip 传空（K8s 不用）
            let registered = self
                .register_pingora_backends(&status.app_id, &http_ports, "")
                .await;
            if !registered.is_empty() {
                count += 1;
            }
        }
        info!(
            "[APP] pingora backends rebuilt: {count} apps ({} managed apps total in cluster)",
            statuses.len()
        );
        Ok(())
    }

    /// 获取应用目录（rcoder 视角）。
    ///
    /// - K8s per-app: `resolve_workspace_path` 拿 per-app subvolume 聚合路径
    ///   (`{cephfs_root}/{subvolumePath}` = per-app PVC 根); UserApp pod 挂 per-app PVC 根到 /app
    ///   (subPath=None), 故 rcoder 写 PVC 根 (不 join app_id)。
    /// - Docker/无 Ceph: resolve 返回 None → 共享 `workspace_root/{app_id}` (= apps/{app_id},
    ///   运行时适配, 非 per-app 失败)。
    /// - K8s per-app resolve 失败 (Err): **Fail Fast** 返回 Backend 错误, 不 fallback 共享
    ///   (避免 per-app PVC + 共享 PVC 数据面分裂, 见 code-review M1/M2)。
    pub(super) async fn get_container_app_dir(&self, app_id: &str) -> AppResult<PathBuf> {
        match self
            .runtime
            .resolve_workspace_path(app_id, &ServiceType::UserApp)
            .await
        {
            Ok(Some(base)) => Ok(PathBuf::from(base)), // K8s per-app PVC 根 (不 join app_id)
            Ok(None) => Ok(PathBuf::from(self.config.get_workspace_root()).join(app_id)), // Docker 共享 Local
            Err(e) => Err(AppOperationError::Backend(format!(
                "UserApp per-app PVC resolve 失败 (app_id={app_id}): {e} — 检查 cephfs-root 挂载 + PVC Bound 状态"
            ))),
        }
    }

    /// ensure UserApp per-app 工作空间就绪 (K8s): ensure PVC 带 requests.storage 用户配额 + 重试
    /// resolve 等 ceph-csi provision subvolumePath (SC Immediate 后秒级, 慢可达 10s+)。
    ///
    /// 必须在 create_app_dirs (建目录) + create_deployment (Docker bind mount 需源目录存在) 之前调用:
    /// - K8s: ensure PVC 带配额 + 等 subvolumePath → 后续 create_app_dirs resolve per-app 成功,
    ///   建 code/data/logs 在 per-app PVC 根 (app pod 挂同 PVC, 无分裂); create_deployment 命中
    ///   PVC active 复用 (配额不丢, 因首次 ensure 已带配额)。
    /// - Docker: 无 per-app PVC → no-op (create_app_dirs 走共享 Local, create_deployment bind mount)。
    async fn ensure_app_workspace_ready(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> AppResult<()> {
        if !shared_types::is_kubernetes_runtime() {
            return Ok(()); // Docker 无 per-app PVC
        }
        self.runtime
            .ensure_workspace(app_id, &ServiceType::UserApp, storage_size)
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("ensure UserApp PVC (app_id={app_id}): {e}"))
            })?;
        // 重试 resolve 等 ceph-csi provision subvolumePath 填充 (PVC Bound 后 PV subvolumePath 仍有延迟)
        const MAX_RETRIES: u32 = 15;
        let mut attempt: u32 = 0;
        loop {
            match self
                .runtime
                .resolve_workspace_path(app_id, &ServiceType::UserApp)
                .await
            {
                Ok(Some(_)) | Ok(None) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt < MAX_RETRIES {
                        tracing::debug!(
                            "[APP] UserApp PVC subvolumePath pending ({}/{}, app_id={}): {}",
                            attempt,
                            MAX_RETRIES,
                            app_id,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    } else {
                        return Err(AppOperationError::Backend(format!(
                            "UserApp PVC subvolumePath 未就绪 (app_id={app_id}, 重试 {MAX_RETRIES} 次): {e}"
                        )));
                    }
                }
            }
        }
    }

    /// 获取应用目录的宿主机路径（Docker bind mount 源）
    ///
    /// Docker 模式：rcoder 通常也运行在容器内，需经 HostPathResolver 将容器内路径
    /// 转为宿主机路径；解析失败回退到原路径。K8s 模式此值不被使用 (subPath)。
    ///
    /// 注意: get_container_app_dir 现返回 Result (K8s per-app 失败 Fail Fast)。本函数保持
    /// PathBuf 签名 (build_container_params 不感知错误), K8s 模式此值本就不用, resolve 失败时
    /// 降级共享路径即可; Docker 模式 resolve Ok(None) → 共享 (正常)。
    async fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let p = self
            .get_container_app_dir(app_id)
            .await
            .unwrap_or_else(|_| PathBuf::from(self.config.get_workspace_root()).join(app_id));
        if let Some(resolver) = self.path_resolver.get("default") {
            resolver.resolve_to_host_path(&p).unwrap_or(p)
        } else {
            p
        }
    }

    /// 创建应用工作空间子目录（code/data/logs）。在 ensure_app_workspace_ready (K8s ensure PVC +
    /// 等 subvolumePath) 之后、create_deployment (Docker bind mount 需源目录存在) 之前调用。
    /// Docker: 共享 Local; K8s: per-app PVC 根 (ensure_app_workspace_ready 已确保 resolve 成功)。
    async fn create_app_dirs(&self, app_id: &str) -> AppResult<()> {
        let app_dir = self.get_container_app_dir(app_id).await?;
        fs::create_dir_all(app_dir.join("code"))
            .await
            .map_err(|e| map_io_error("failed to create code dir", e, false))?;
        fs::create_dir_all(app_dir.join("data"))
            .await
            .map_err(|e| map_io_error("failed to create data dir", e, false))?;
        fs::create_dir_all(app_dir.join("logs"))
            .await
            .map_err(|e| map_io_error("failed to create logs dir", e, false))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl super::AppServiceTrait for AppService {
    async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        self.create_app(request).await
    }

    async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>> {
        self.query_apps(request).await
    }

    async fn list_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        self.list_app_runtimes().await
    }

    async fn get_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.get_app(app_id).await
    }

    async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.update_app(app_id, request).await
    }

    async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()> {
        self.delete_app(app_id, purge, expected_resource_version)
            .await
    }

    async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo> {
        self.get_app_storage(app_id).await
    }

    async fn delete_app_storage(&self, app_id: &str) -> AppResult<()> {
        self.delete_app_storage(app_id).await
    }

    async fn reset_db_password(
        &self,
        app_id: &str,
        request: ResetDbPasswordRequest,
    ) -> AppResult<()> {
        self.reset_db_password(app_id, request).await
    }

    async fn create_database(
        &self,
        app_id: &str,
        request: CreateDatabaseRequest,
    ) -> AppResult<()> {
        self.create_database(app_id, request).await
    }

    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        self.query_storage(request).await
    }

    async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.start_app(app_id).await
    }

    async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.stop_app(app_id).await
    }

    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.restart_app(app_id).await
    }

    async fn get_app_logs(&self, app_id: &str, params: LogParams) -> AppResult<Vec<LogEntry>> {
        self.get_app_logs(app_id, params).await
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> AppResult<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>
    {
        self.stream_app_logs(app_id, tail).await
    }

    async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        self.get_app_stats(app_id).await
    }

    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        self.get_app_events(app_id).await
    }

    async fn get_app_file_logs(
        &self,
        app_id: &str,
        file_path: &str,
        tail: u32,
    ) -> AppResult<Vec<LogEntry>> {
        self.get_app_file_logs(app_id, file_path, tail).await
    }

    async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        self.upload_file(app_id, file_data, target, flatten).await
    }

    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> AppResult<Vec<FileInfo>> {
        self.list_files(app_id, subpath).await
    }

    async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        self.delete_file(app_id, file_path).await
    }
}
