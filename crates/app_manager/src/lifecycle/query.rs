//! UserApp 查询面（从 service.rs 拆出，extension-impl）。
//!
//! list_app_runtimes（对账）/ query_apps（分页过滤）/ get_app（详情）——
//! 列表类查询经 TTL 缓存（防轮询穿透到 Docker daemon/K8s apiserver，
//! daemon 无响应曾把调用方整个挂死）；get_app 详情为单 app 直查不缓存。

use std::time::Duration;

use garde::Validate as _;
use tracing::{instrument, warn};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 列表缓存 TTL：管理界面轮询频率量级内的最大陈旧窗口；写操作
    /// （create/delete/update/start）会主动失效，实际一致性窗口更小。
    const DEPLOY_LIST_TTL: Duration = Duration::from_secs(3);
    /// 穿透查询的超时兜底：daemon/apiserver 无响应时快速报错，
    /// 而不是把 HTTP 请求挂死到连接超时（Docker daemon 高负载实战）。
    const DEPLOY_LIST_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

    /// 对账接口：列出该 owner 归属的应用运行时状态（metadata owner 匹配；
    /// 无归属记录的应用不返回——分区归属口径）。
    #[instrument(skip(self))]
    pub async fn list_app_runtimes(&self, user_id: &str) -> AppResult<Vec<AppRuntimeInfo>> {
        let statuses = self.list_deployments_cached().await?;
        let (owned, unowned): (Vec<_>, Vec<_>) = statuses.into_iter().partition(|s| {
            self.metadata
                .lookup(&s.app_id)
                .is_some_and(|m| m.user_id.as_deref() == Some(user_id))
        });
        if !unowned.is_empty() {
            tracing::debug!(
                owner = user_id,
                skipped = unowned.len(),
                "list_app_runtimes skipped apps without matching owner metadata"
            );
        }
        Ok(owned
            .into_iter()
            .map(|s| self.build_runtime_info(s))
            .collect())
    }

    /// 无过滤全量版（系统内部扫描面：闲置回收器须覆盖所有 app，与归属无关）。
    #[instrument(skip(self))]
    pub async fn list_all_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        let statuses = self.list_deployments_cached().await?;
        Ok(statuses
            .into_iter()
            .map(|s| self.build_runtime_info(s))
            .collect())
    }

    /// `list_deployments` 的缓存版（query_apps / list_app_runtimes 共用）：
    /// TTL 内直接返回快照；过期穿透查询——持 tokio Mutex 期间并发请求等待，
    /// 天然 single-flight 防击穿（只有一个请求打到 daemon）；穿透带超时。
    async fn list_deployments_cached(
        &self,
    ) -> AppResult<Vec<container_runtime_api::DeploymentStatus>> {
        let mut guard = self.deploy_list_cache.lock().await;
        if let Some(entry) = guard.as_ref()
            && entry.fetched_at.elapsed() < Self::DEPLOY_LIST_TTL
        {
            return Ok(entry.items.clone());
        }
        let statuses = tokio::time::timeout(
            Self::DEPLOY_LIST_QUERY_TIMEOUT,
            self.runtime.list_deployments(),
        )
        .await
        .map_err(|_| {
            AppOperationError::Backend(format!(
                "list deployments timed out after {}s (docker daemon / apiserver unresponsive?)",
                Self::DEPLOY_LIST_QUERY_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?;
        *guard = Some(crate::service::DeployListCacheEntry {
            fetched_at: tokio::time::Instant::now(),
            items: statuses.clone(),
        });
        Ok(statuses)
    }

    /// 查询缓存失效（写路径调用：create/delete/update/start 等改变
    /// Deployment 集合或状态的操作成功后）。
    pub(crate) async fn invalidate_deploy_cache(&self) {
        *self.deploy_list_cache.lock().await = None;
    }

    /// 查询应用列表（实时查集群 + 过滤/分页）
    #[instrument(skip(self, request))]
    pub async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>> {
        request.validate().map_err(|e| {
            let msg = e
                .iter()
                .map(|(p, err)| format!("{p}: {}", err.message()))
                .collect::<Vec<_>>()
                .join("; ");
            AppOperationError::Validation(msg)
        })?;
        let mut items = self.list_app_runtimes(request.user_id.trim()).await?;

        // 过滤：status/app_ids 为运行时字段直接生效；name/created_at 需业务元数据
        // （集群不持有），仅 PG 模式（metadata 持久化已注入）经内存 join 生效，
        // 纯内存模式维持忽略 + warn（旧行为）。
        if let Some(filters) = &request.filters {
            if let Some(status) = &filters.status {
                items.retain(|app| status.contains(&app.status));
            }
            if let Some(app_ids) = &filters.app_ids {
                items.retain(|app| app_ids.contains(&app.app_id));
            }
            if filters.name.is_some() || filters.created_at.is_some() {
                if self.metadata.persistence().is_some() {
                    let name = filters.name.as_deref();
                    // DateRange RFC3339 解析失败 → 400（过滤已生效，非法参数应被告知）
                    let range = match &filters.created_at {
                        Some(range) => {
                            let start = chrono::DateTime::parse_from_rfc3339(&range.start)
                                .map_err(|e| {
                                    AppOperationError::Validation(format!(
                                        "invalid created_at.start '{}': {e}",
                                        range.start
                                    ))
                                })?
                                .with_timezone(&chrono::Utc);
                            let end = chrono::DateTime::parse_from_rfc3339(&range.end)
                                .map_err(|e| {
                                    AppOperationError::Validation(format!(
                                        "invalid created_at.end '{}': {e}",
                                        range.end
                                    ))
                                })?
                                .with_timezone(&chrono::Utc);
                            Some((start, end))
                        }
                        None => None,
                    };
                    items.retain(|app| {
                        let Some(meta) = self.metadata.lookup(&app.app_id) else {
                            // 无元数据记录的 app（非 PG 时代创建）不满足 name/created_at 过滤
                            return false;
                        };
                        // name 模糊匹配（contains，与 models 注释"按名称模糊搜索"对齐；
                        // 此前精确匹配导致部分名称查询恒 0 条且无提示）
                        name.is_none_or(|n| meta.name.as_deref().is_some_and(|v| v.contains(n)))
                            && range.is_none_or(|(start, end)| {
                                meta.created_at >= start && meta.created_at <= end
                            })
                    });
                } else {
                    warn!(
                        "[APP] query_apps name/created_at filters require business metadata (PG mode), ignored"
                    );
                }
            }
        }

        // 排序（app_id 直接可用；name/created_at 经 metadata join，缺元数据排最后；默认升序）
        if let Some(sort_by) = &request.sort_by {
            match sort_by.as_str() {
                "app_id" => {
                    items.sort_by(|a, b| a.app_id.cmp(&b.app_id));
                }
                "name" => {
                    if self.metadata.persistence().is_none() {
                        warn!("[APP] sort_by=name requires business metadata (PG mode), no-op");
                    }
                    items.sort_by_key(|app| {
                        self.metadata
                            .lookup(&app.app_id)
                            .and_then(|m| m.name)
                            .unwrap_or_default()
                    });
                }
                "created_at" => {
                    if self.metadata.persistence().is_none() {
                        warn!(
                            "[APP] sort_by=created_at requires business metadata (PG mode), no-op"
                        );
                    }
                    // (缺元数据排最后, 时间升序)：bool false < true 保证有元数据的排前
                    items.sort_by_key(|app| {
                        let meta = self.metadata.lookup(&app.app_id);
                        (meta.is_none(), meta.map(|m| m.created_at))
                    });
                }
                // 非法值 400（此前落入 `_ => {}` 不排序，但随后的 reverse 仍执行——
                // 传 created_at 等未支持值+desc 会把默认顺序直接反转，半生效的静默错误）
                other => {
                    return Err(AppOperationError::Validation(format!(
                        "sort_by must be one of app_id/name/created_at, got '{other}'"
                    )));
                }
            }
            if request.sort_order == Some(SortOrder::Desc) {
                items.reverse();
            }
        }

        // 分页（对齐 query_storage/publish tasks 的校验口径：非法值 400 而非静默 clamp——
        // 此前 page 超大在 debug 构建 u32 乘法溢出 panic、release 环绕返回错页数据；
        // page_size=0 算出 total_pages=42 亿）
        let page = request.page.unwrap_or(1);
        let page_size = request.page_size.unwrap_or(20);
        if page < 1 {
            return Err(AppOperationError::Validation(
                "page must be >= 1".to_string(),
            ));
        }
        if !(1..=100).contains(&page_size) {
            return Err(AppOperationError::Validation(
                "page_size must be within 1..=100".to_string(),
            ));
        }
        let total = items.len() as u64;
        // u64 中间量防溢出（合法输入下 (page-1)*page_size 最大 ~4.3e11，超 usize 的
        // 极端页码截断为越界空页而非 panic/环绕）
        let start = ((page as u64 - 1) * page_size as u64) as usize;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn deployed(runtime: &MockRuntime, app_id: &str) {
        runtime.deployments.insert(
            app_id.to_string(),
            container_runtime_api::DeploymentStatus {
                app_id: app_id.to_string(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".to_string(),
                ..Default::default()
            },
        );
    }

    /// 测试辅助：为 app 注册 owner 后按该 owner 查询（owner 过滤是
    /// list_app_runtimes 的前置语义，缓存断言不受影响）。返回查询条数。
    async fn owned_list(svc: &AppService, app_id: &str, user_id: &str) -> usize {
        svc.metadata
            .record(app_id, None, Some(user_id.to_string()), None, None)
            .await;
        svc.list_app_runtimes(user_id).await.unwrap().len()
    }

    /// TTL 内命中缓存：多次查询只穿透一次到 runtime。
    #[tokio::test]
    async fn deploy_list_cache_hits_within_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        deployed(&runtime, "app-a");
        let svc = test_service(tmp.path(), runtime.clone());

        let r1 = owned_list(&svc, "app-a", "u1").await;
        let r2 = owned_list(&svc, "app-a", "u1").await;
        assert_eq!(r1, 1);
        assert_eq!(r2, 1);
        assert_eq!(
            runtime.list_calls.load(Ordering::Relaxed),
            1,
            "second query within TTL must hit cache"
        );
    }

    /// 写路径失效：invalidate 后下一次查询重新穿透。
    #[tokio::test]
    async fn deploy_list_cache_invalidated_by_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        deployed(&runtime, "app-a");
        let svc = test_service(tmp.path(), runtime.clone());

        owned_list(&svc, "app-a", "u1").await;
        deployed(&runtime, "app-b"); // 模拟并发新建（绕过 service 写路径）
        svc.invalidate_deploy_cache().await;
        // 两个 app 都注册给同一 owner 后对账
        svc.metadata
            .record("app-b", None, Some("u1".to_string()), None, None)
            .await;
        let r = svc.list_app_runtimes("u1").await.unwrap();
        assert_eq!(r.len(), 2, "invalidated cache must refetch");
        assert_eq!(runtime.list_calls.load(Ordering::Relaxed), 2);
    }

    /// TTL 过期重查（时间推进用真实 sleep 的短替代：直接操纵缓存时间戳）。
    #[tokio::test]
    async fn deploy_list_cache_expires_after_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        deployed(&runtime, "app-a");
        let svc = test_service(tmp.path(), runtime.clone());

        owned_list(&svc, "app-a", "u1").await;
        // 把缓存时间戳拨回 TTL 之前，模拟过期
        {
            let mut guard = svc.deploy_list_cache.lock().await;
            if let Some(entry) = guard.as_mut() {
                entry.fetched_at = tokio::time::Instant::now()
                    - (AppService::DEPLOY_LIST_TTL + Duration::from_secs(1));
            }
        }
        owned_list(&svc, "app-a", "u1").await;
        assert_eq!(
            runtime.list_calls.load(Ordering::Relaxed),
            2,
            "expired cache must refetch"
        );
    }
}
