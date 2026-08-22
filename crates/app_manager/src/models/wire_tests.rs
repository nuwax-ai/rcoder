//! UserApp API DTO 的 wire 契约锁定测试（snake_case 字段 + 小写枚举值）。
//!
//! 动机：DTO 曾经历 camelCase↔snake_case 两次方向切换，且 `AppPortStatus` 曾漏撤
//! camelCase（d3ba55e）——models 层此前没有任何 JSON 序列化测试，wire 形态漂移
//! 只能靠 e2e/人工发现。本模块按"对外契约"逐域锁定：字段 snake、枚举值小写、
//! 存量读兼容（ReleaseStatus 的 PascalCase alias）。新增对外 DTO 时应在此补锁定。

#[cfg(test)]
mod tests {
    use crate::models::commons::{ExposeType, HealthCheckType};
    use crate::models::release::{ActivateReleaseRequest, ReleaseInfo, ReleaseStatus};
    use crate::models::request::{
        AppFilters, CreateAppRequest, QueryAppsRequest, SortOrder, UpdateAppRequest,
    };
    use crate::models::response::{AppRuntimeInfo, PaginatedResponse, Pagination};
    use crate::models::storage::{QueryStorageRequest, StorageFilters, StorageInfo};

    fn assert_no_camel(json: &serde_json::Value, ctx: &str) {
        fn walk(v: &serde_json::Value, keys: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        if k.chars().any(|c| c.is_ascii_uppercase()) {
                            keys.push(k.clone());
                        }
                        walk(val, keys);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, keys);
                    }
                }
                _ => {}
            }
        }
        let mut camel_keys = Vec::new();
        walk(json, &mut camel_keys);
        assert!(
            camel_keys.is_empty(),
            "{ctx}: camelCase 键残留 {camel_keys:?} in {json}"
        );
    }

    /// create 请求：snake 键反序列化 + 枚举值小写 + deny 不可见的默认行为。
    #[test]
    fn create_app_request_accepts_snake_wire() {
        let req: CreateAppRequest = serde_json::from_value(serde_json::json!({
            "app_id": "app-order-svc",
            "name": "订单服务",
            "user_id": "u6",
            "image": "registry.example/app-runtime:1",
            "command": ["java", "-jar", "app.jar"],
            "env": {"APP_MODE": "prod"},
            "ports": [
                {"name": "http", "port": 8080, "expose_type": "http"},
                {"name": "db", "port": 5432, "expose_type": "tcp"}
            ],
            "health_check": {"check_type": "http", "path": "/actuator/health", "port": 8080},
            "tenant_id": "t1",
            "space_id": "s1",
            "idle_timeout_seconds": 600
        }))
        .expect("snake request wire must deserialize");
        assert_eq!(req.app_id.as_deref(), Some("app-order-svc"));
        assert_eq!(req.user_id, "u6");
        let ports = req.ports.as_ref().expect("ports");
        assert_eq!(ports.len(), 2);
        assert!(matches!(ports[0].expose_type, ExposeType::Http));
        assert!(matches!(ports[1].expose_type, ExposeType::Tcp));
        let hc = req.health_check.as_ref().expect("health_check");
        assert!(matches!(hc.check_type, HealthCheckType::Http));
        assert_eq!(req.idle_timeout_seconds, Some(600));
    }

    /// update 请求：可更新面（env/secrets/resources 等）snake 键 + 已删字段向后兼容。
    #[test]
    fn update_app_request_accepts_snake_wire() {
        let req: UpdateAppRequest = serde_json::from_value(serde_json::json!({
            "image": "registry.example/app-runtime:2",
            "expected_resource_version": "rv-1"
        }))
        .expect("partial update snake wire");
        assert_eq!(req.image.as_deref(), Some("registry.example/app-runtime:2"));
        assert!(req.env.is_none() && req.secrets.is_none() && req.resources.is_none());

        let full: UpdateAppRequest = serde_json::from_value(serde_json::json!({
            "image": "i",
            "env": {"A": "1"},
            "recycle_enabled": false
        }))
        .expect("full update snake wire");
        assert!(full.recycle_enabled.is_some());
        assert_eq!(
            full.env.as_ref().and_then(|e| e.get("A")),
            Some(&"1".to_string())
        );

        // v2 四要素内定后 command/ports/health_check 已从请求面删除——旧调用方
        // 仍传这些键时必须静默忽略（无 deny_unknown_fields），而非 422 反序列化失败。
        let legacy: Result<UpdateAppRequest, _> = serde_json::from_value(serde_json::json!({
            "image": "i",
            "command": ["java", "-jar", "app.jar"],
            "ports": [{"name": "http", "port": 8080, "expose_type": "http"}],
            "health_check": {"check_type": "http", "path": "/health", "port": 8080}
        }));
        let legacy = legacy.expect("legacy callers sending removed fields must not break");
        assert_eq!(legacy.image.as_deref(), Some("i"));
    }

    /// query 请求：filters/sort 键与 SortOrder 小写值。
    #[test]
    fn query_apps_request_accepts_snake_wire() {
        let req: QueryAppsRequest = serde_json::from_value(serde_json::json!({
            "page": 1,
            "page_size": 20,
            "filters": {
                "status": ["running", "error"],
                "app_ids": ["app-1"],
                "name": "订单"
            },
            "sort_by": "created_at",
            "sort_order": "desc"
        }))
        .expect("query snake wire");
        assert!(matches!(req.sort_order, Some(SortOrder::Desc)));
        let filters = req.filters.expect("filters");
        assert!(
            matches!(filters, AppFilters { ref app_ids, .. } if app_ids.as_deref() == Some(&["app-1".to_string()][..]))
        );
    }

    /// 响应主形态 AppRuntimeInfo：反序列化构造 + 序列化断言——嵌套 ports/access/
    /// conditions 全 snake（AppPortStatus 曾漏撤 camelCase 正是缺此锁定）+ status 值小写。
    #[test]
    fn app_runtime_info_round_trips_snake_wire() {
        let info: AppRuntimeInfo = serde_json::from_value(serde_json::json!({
            "app_id": "app-x",
            "status": "running",
            "phase": "Running",
            "message": null,
            "replicas": 1,
            "ready_replicas": 1,
            "restart_count": 0,
            "pod_ip": "10.0.0.1",
            "node": null,
            "started_at": null,
            "ports": [{
                "name": "http", "port": 8080,
                "expose_type": "http", "external_port": 30080
            }],
            "access": {
                "external": {"http": "https://gw/apps/app-x", "tcp": [
                    {"name": "db", "node_port": 31999, "access_url": "tcp://node:31999"}
                ]},
                "internal": {"domain": "svc.ns.svc.cluster.local",
                    "short_domain": "svc.ns", "ports": [{"name": "http", "port": 8080}]}
            },
            "conditions": [],
            "health": {"status": "Running", "instance": null, "probes": null},
            "resource_version": "rv-1"
        }))
        .expect("runtime info snake wire deserializes");
        assert!(matches!(
            info.status,
            crate::models::commons::AppStatus::Running
        ));

        let v = serde_json::to_value(&info).expect("serialize runtime info");
        assert_eq!(v["status"], "running", "AppStatus 值小写");
        assert_eq!(v["ready_replicas"], 1);
        assert_eq!(v["pod_ip"], "10.0.0.1");
        assert_eq!(v["ports"][0]["expose_type"], "http");
        assert_eq!(v["ports"][0]["external_port"], 30080);
        assert_eq!(
            v["access"]["internal"]["short_domain"], "svc.ns",
            "嵌套 InternalAccess snake"
        );
        assert_no_camel(&v, "AppRuntimeInfo");
    }

    /// 分页响应（tasks/query 等复用）：page_size/total_pages snake。
    #[test]
    fn paginated_response_serializes_snake_wire() {
        let page: PaginatedResponse<String> = PaginatedResponse {
            items: vec!["a".into()],
            pagination: Pagination {
                page: 1,
                page_size: 20,
                total: 1,
                total_pages: 1,
            },
        };
        let v = serde_json::to_value(&page).expect("serialize page");
        assert_eq!(v["pagination"]["page_size"], 20);
        assert_eq!(v["pagination"]["total_pages"], 1);
        assert_no_camel(&v, "PaginatedResponse");
    }

    /// ReleaseInfo：字段 snake；ReleaseStatus 写小写、读兼容 PascalCase 存量 index.json。
    #[test]
    fn release_info_snake_wire_and_status_alias() {
        let info = ReleaseInfo {
            release_id: "rel-1".into(),
            sha256: "0".repeat(64),
            size_bytes: 1024,
            status: ReleaseStatus::Active,
            created_at: "2026-08-20T00:00:00Z".into(),
            activated_at: Some("2026-08-20T01:00:00Z".into()),
            failure_message: None,
        };
        let v = serde_json::to_value(&info).expect("serialize release");
        assert_eq!(v["status"], "active", "写出一律小写");
        assert_eq!(v["release_id"], "rel-1");
        assert_eq!(v["size_bytes"], 1024);
        assert_no_camel(&v, "ReleaseInfo");

        // 存量 index.json（camelCase 键已不存在场景外的 PascalCase 值）读兼容：
        // caef1f5 前的 index 存 "Active"/"Failed"，alias 保证升级可读
        let legacy = serde_json::from_value::<ReleaseInfo>(serde_json::json!({
            "releaseId": "rel-1", "sha256": "0", "sizeBytes": 1,
            "status": "Active", "createdAt": "t"
        }));
        // 字段层不做旧 camel 兼容（清数据直切决策）——仅值层 alias 生效：
        // 构造只差值形态的 snake 载荷验证 alias
        assert!(legacy.is_err(), "字段层旧 camel 键按决策不兼容（需清数据）");
        let snake_legacy_value = serde_json::from_value::<ReleaseInfo>(serde_json::json!({
            "release_id": "rel-1", "sha256": "0", "size_bytes": 1,
            "status": "Active", "created_at": "t"
        }))
        .expect("PascalCase 值经 alias 读兼容");
        assert!(matches!(snake_legacy_value.status, ReleaseStatus::Active));
    }

    /// activate 请求：readiness_timeout_seconds snake 键。
    #[test]
    fn activate_request_accepts_snake_wire() {
        let req: ActivateReleaseRequest = serde_json::from_value(serde_json::json!({
            "readiness_timeout_seconds": 300
        }))
        .expect("activate snake wire");
        assert_eq!(req.readiness_timeout_seconds, Some(300));
    }

    /// storage 查询：orphan_only/app_ids snake + StorageInfo 响应 is_orphan。
    #[test]
    fn storage_dto_snake_wire() {
        let req: QueryStorageRequest = serde_json::from_value(serde_json::json!({
            "page": 1,
            "page_size": 20,
            "filters": {"orphan_only": true, "app_ids": ["app-1"]}
        }))
        .expect("storage query snake wire");
        assert!(matches!(
            req.filters,
            Some(StorageFilters {
                orphan_only: Some(true),
                ..
            })
        ));

        let info = StorageInfo {
            app_id: "app-1".into(),
            exists: true,
            path: "/data/app-1".into(),
            modified_at: Some("2026-08-20T00:00:00Z".into()),
            is_orphan: false,
        };
        let v = serde_json::to_value(&info).expect("serialize storage info");
        assert_eq!(v["is_orphan"], false);
        assert_eq!(v["modified_at"], "2026-08-20T00:00:00Z");
        assert_no_camel(&v, "StorageInfo");
    }
}
