//! Userapp desired-state 变更面（从 service.rs 拆出，extension-impl）。
//!
//! update_app（SSA 全量替换+live 回退）/ delete_app（默认保留数据面，purge 清空）。

use std::sync::Arc;

use tracing::{debug, info, instrument};

use container_runtime_api::{DeploymentStatus, ExposeType as RtExposeType, StorageResizeOutcome};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
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
        // 与发布串行（同 create/delete 的 per-app 进程级发布锁），但**不排队傻等**——
        // activate 等就绪可达 30 分钟，update 等它没有意义；锁被占（发布进行中）立即
        // 409 让调用方稍后重试。delete 保持阻塞等待语义（清理动作，等一下无妨）。
        // 无并发发布时锁条目可能不存在 → entry 建立并立刻拿到（try 必成功）。
        let lock_arc = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        let _update_lock = lock_arc.try_lock_owned().map_err(|_| {
            AppOperationError::Conflict(format!(
                "app {app_id} is being activated/published, retry after it finishes"
            ))
        })?;
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
        let params = self
            .build_container_params_from_update(app_id, &request, &current)
            .await?;
        // storage 扩容前置（pingora unregister 之前——失败零副作用直接返回）：
        // K8s 下 resources.storage 是 per-app PVC 扩容目标（仅扩不缩、在线生效
        // 不重建 Pod）；Docker no-op。失败阻断整个 update——该字段对外承诺生效，
        // 静默降级会让调用方以为已扩容。
        if let Some(new_size) = params.storage_size.as_deref() {
            match self.runtime.resize_app_storage(app_id, new_size).await {
                Ok(StorageResizeOutcome::Resized { from, to }) => {
                    info!(
                        "[APP] storage resized app_id={app_id}: {from} -> {to} (external-resizer async)"
                    );
                }
                Ok(StorageResizeOutcome::AlreadyEqual) => {
                    info!(
                        "[APP] storage resize no-op app_id={app_id}: requested {new_size} equals current"
                    );
                }
                Ok(StorageResizeOutcome::Noop) => {
                    debug!(
                        "[APP] storage resize no-op (runtime without PVC capacity) app_id={app_id}: {new_size}"
                    );
                }
                Ok(StorageResizeOutcome::ShrinkRejected {
                    current: cur,
                    requested,
                }) => {
                    return Err(AppOperationError::Validation(format!(
                        "K8s PVC supports expansion only: app {app_id} requested {requested} < current {cur}"
                    )));
                }
                Err(e) => {
                    return Err(map_runtime_error(
                        &format!("[APP] resize_app_storage failed app_id={app_id}"),
                        e,
                    ));
                }
            }
        }
        // 恢复依据先取出（unregister 会移除注册表条目）：pingora_ports 里的是当前
        // 实际生效的 Http 端口——比 current.ports 反推可靠（Docker 后端的状态 ports
        // 只含 TCP，反推恒空会让恢复分支注册了个寂寞）。
        let registered_http_ports = self.registered_http_ports(app_id);
        // 先注销旧 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 下方按本次 http_ports 重新注册到 Service FQDN，注销-重注成对保证一致）。
        self.unregister_pingora_backends(app_id).await;
        // http_ports 在 move 前从 params 提取：优先本次回退后的完整 ports（live 回退
        // 后含全部端口的权威 desired）；读失败降级（params.ports=None）时退当前注册值。
        let http_ports: Vec<u16> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| matches!(p.expose_type, RtExposeType::Http))
                    .map(|p| p.port)
                    .collect()
            })
            .unwrap_or(registered_http_ports);
        let info = match self.runtime.patch_deployment(params).await {
            Ok(info) => info,
            Err(e) => {
                // patch 失败：Deployment 原样仍在运行，恢复 pingora 路由（对齐 delete_app
                // 的失败恢复分支）——否则应用还在跑但 /api/v1/userapp/proxy/app/prod/{id} 502，直到
                // 下次成功 update 或进程重启。
                let previous_host = current.pod_ip.clone().unwrap_or_default();
                self.register_pingora_backends(app_id, &http_ports, &previous_host)
                    .await;
                return Err(map_runtime_error(
                    &format!("[APP] patch_deployment failed app_id={app_id}"),
                    e,
                ));
            }
        };
        // 重新注册 Pingora backend（与上面 unregister 对称——否则部分更新会丢
        // Pingora 路由，app 经 /api/v1/userapp/proxy/app/prod/{id} 变 502）。
        // 注：register 在 K8s 模式并非 no-op，会把 backend 指到 Service FQDN（与 create 一致）。
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] app updated: {}", app_id);
        // 业务元数据 upsert（created_at SQL 侧不更新）。name 缺省回退已存值——
        // update 语义里 name 是"仅元数据"调用方常不带,upsert 是整字段覆盖,
        // 直传 None 会把业务名清空（query name 过滤随之失效）。tenant/space
        // 保持与 label 相同的"携带即覆盖"语义（create 时的值不回退）。
        let name = request
            .name
            .clone()
            .or_else(|| self.metadata.lookup(app_id).and_then(|meta| meta.name));
        // user_id 仅 create 落值（update 请求不带），回填已存值防 upsert 覆盖清空
        let user_id = self
            .metadata
            .lookup(app_id)
            .and_then(|meta| meta.user_id.clone());
        self.metadata
            .record(
                app_id,
                name,
                user_id,
                request.tenant_id.clone(),
                request.space_id.clone(),
            )
            .await;
        drop(_update_lock);
        self.remove_unused_process_release_lock(app_id);
        self.invalidate_deploy_cache().await;
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
        let previous = self.fetch_runtime_status_or_err(app_id).await?;
        // 乐观锁（同 update_app）：expected 不匹配 → 409 Conflict
        if let Some(expected) = expected_resource_version
            && let Some(actual) = &previous.resource_version
            && expected != actual
        {
            return Err(AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            )));
        }
        // delete/purge 必须与 prepare/activate/confirm/delete-release 串行，避免删除 PVC
        // 时另一个任务仍在写版本包或切换 code。
        let release_lock = self.acquire_process_release_lock(app_id).await;
        info!("[APP] deleting app: {} (purge={})", app_id, purge);

        // 1. 删除计算面（防护序列与失败对称恢复见 tear_down_compute_plane）
        self.tear_down_compute_plane(app_id, &previous).await?;

        // 2. purge=true 必须销毁持久存储（K8s: PVC + Ceph subvolume；Docker:
        //    workspace 目录），与 API 的“全部删除”语义一致。仅清空目录却保留 PVC
        //    会继续占用配额，并让成功响应与实际状态不一致。
        //    元数据行**保留**（三档语义：delete/purge 保留行支持误删找回，仅独立
        //    storage/destroy 接口删行）。
        if purge {
            self.destroy_app_storage_keep_metadata(app_id, app_id)
                .await?;
            info!("[APP] persistent storage destroyed: {}", app_id);
        } else {
            info!(
                "[APP] retained persistent storage (pass purge=true to clear): {}",
                app_id
            );
        }

        drop(release_lock);
        self.remove_unused_process_release_lock(app_id);
        self.invalidate_deploy_cache().await;
        Ok(())
    }

    /// 删除 prod 计算面防护序列（delete_app 与 purge_app 共用）：
    /// 取注册端口 → 注销 Pingora backend → 阻断流量唤醒 → delete_deployment
    /// （失败：恢复原活动状态 + 按原 pod_ip 重注册 backend 后透传）→ forget_app。
    ///
    /// 持锁约定：调用方须已持有 per-app 发布锁——本方法及其调用链**不重入**
    /// acquire_process_release_lock（重试无死锁保证）。`previous` 由调用方
    /// 持有（delete_app 乐观锁需要；purge_app 幂等分派需要）。
    pub(crate) async fn tear_down_compute_plane(
        &self,
        app_id: &str,
        previous: &DeploymentStatus,
    ) -> AppResult<()> {
        // 失败恢复依据先取出（unregister 会移除注册表条目）：pingora_ports 里的是
        // 当前实际生效的 Http 端口——比 previous.ports 反推可靠（Docker 后端的状态
        // ports 只含 TCP，反推恒空会让恢复分支注册了个寂寞）。
        let previous_wake_on_traffic = previous
            .wake_on_traffic
            .unwrap_or_else(|| !self.activity.is_wake_blocked(app_id));
        let registered_http_ports = self.registered_http_ports(app_id);
        // 先注销 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 侧 backend 指向 Service FQDN，删除后集群资源整体消失）。
        self.unregister_pingora_backends(app_id).await;
        // 删除计算资源（K8s: Deployment/Service/HTTPRoute/NodePort/ConfigMap/Secret
        // + label orphan 扫描兜底；Docker: 容器）。持久存储默认保留。
        // 先阻止并发流量唤醒；删除失败时恢复原活动状态。
        self.activity.mark_wake_blocked(app_id);
        if let Err(error) = self.runtime.delete_deployment(app_id).await {
            self.restore_activity_state(app_id, previous, previous_wake_on_traffic);
            self.register_pingora_backends(
                app_id,
                &registered_http_ports,
                previous.pod_ip.as_deref().unwrap_or_default(),
            )
            .await;
            return Err(map_runtime_error(
                &format!("[APP] delete_deployment failed app_id={app_id}"),
                error,
            ));
        }
        self.activity.forget_app(app_id);
        Ok(())
    }
}
