//! UserApp 查询面（从 service.rs 拆出，extension-impl）。
//!
//! list_app_runtimes（对账）/ query_apps（分页过滤）/ get_app（详情）——全部实时查集群。

use tracing::{instrument, warn};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
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
