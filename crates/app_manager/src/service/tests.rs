    use super::*;
    use std::sync::atomic::Ordering;

    use crate::models::ResourceLimits;
    use crate::test_support::{MockRuntime, release_lock, test_service};
    use container_runtime_api::StorageResizeOutcome;

    pub(crate) fn create_request(app_id: &str) -> CreateAppRequest {
        CreateAppRequest {
            app_id: Some(app_id.to_owned()),
            name: "r2-app".into(),
            user_id: "u-test".to_string(),
            image: Some("registry.example/app-runtime:test".into()),
            command: None,
            env: None,
            secrets: None,
            resources: None,
            ports: None,
            health_check: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
        }
    }

    /// R2：create_app_runtime 失败——断言 delete_deployment 兜底被调用、原始错误原样返回（不被清理覆盖）。
    #[tokio::test]
    pub(crate) async fn create_app_runtime_failure_triggers_best_effort_cleanup() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.create_fails.store(true, Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone());
        // build_container_params 需 code/release.lock.toml，预铺现场
        let app_dir = root.path().join("app-r2");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let error = service
            .create_app(create_request("app-r2"))
            .await
            .expect_err("create_app must fail");

        // 原始错误原样返回（create_deployment 失败的映射，未被清理逻辑覆盖）
        assert!(
            error.to_string().contains("create_deployment failed"),
            "original error must be preserved, got: {error}"
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.delete_calls.load(Ordering::SeqCst),
            1,
            "delete_deployment fallback must be called after create_app_runtime failure"
        );
    }

    /// R2 对照：清理自身失败也不改变原始错误（只 warn）。
    #[tokio::test]
    pub(crate) async fn create_app_cleanup_failure_keeps_original_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.create_fails.store(true, Ordering::SeqCst);
        runtime.delete_fails.store(true, Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone());
        let app_dir = root.path().join("app-r2b");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let error = service
            .create_app(create_request("app-r2b"))
            .await
            .expect_err("create_app must fail");

        assert!(
            error.to_string().contains("create_deployment failed"),
            "original error must not be masked by cleanup failure, got: {error}"
        );
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    }

    /// 回归（userapp_metadata）：update 不带 name（name 是"仅元数据"调用方常省略）
    /// 不得清空已存业务名——否则 query name 过滤对该 app 永久失效。带 name 则覆盖。
    #[tokio::test]
    pub(crate) async fn update_app_without_name_keeps_metadata_name() {
        use crate::models::UpdateAppRequest;

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        // create_app 需要 code/release.lock.toml
        let app_dir = root.path().join("app-meta");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let mut create = create_request("app-meta");
        create.name = "alpha".into();
        service.create_app(create).await.expect("create app");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("alpha".into()),
            "create records name"
        );

        let update_no_name = UpdateAppRequest {
            user_id: "u1".into(),
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        };
        service
            .update_app("app-meta", update_no_name.clone())
            .await
            .expect("update without name");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("alpha".into()),
            "update without name must NOT clear recorded name"
        );

        let mut update_with_name = update_no_name;
        update_with_name.image = Some("registry.example/app-runtime:v3".into());
        update_with_name.name = Some("beta".into());
        service
            .update_app("app-meta", update_with_name)
            .await
            .expect("update with name");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("beta".into()),
            "explicit name overrides"
        );
    }

    /// update 与发布并发：发布锁被占 → 立即 409（不排队傻等 activate 的就绪窗口）。
    #[tokio::test]
    pub(crate) async fn update_app_conflicts_while_release_lock_held() {
        let root = tempfile::tempdir().expect("tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let _publish_lock = service.acquire_process_release_lock("app-busy").await;

        let request = UpdateAppRequest {
            user_id: "u1".into(),
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        };
        let error = service
            .update_app("app-busy", request)
            .await
            .expect_err("update during publish must 409");
        assert!(
            matches!(error, AppOperationError::Conflict(_)),
            "got: {error}"
        );
    }

    /// update 前置：create 一个 running app（fetch_runtime_status 需要 Deployment
    /// 存在），返回 service 与 runtime 句柄（resize/patch 调用断言用）。
    async fn created_app_service(
        root: &std::path::Path,
        app_id: &str,
    ) -> (AppService, Arc<MockRuntime>) {
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root, runtime.clone());
        let app_dir = root.join(app_id);
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        service
            .create_app(create_request(app_id))
            .await
            .expect("create app");
        (service, runtime)
    }

    fn update_request_with_storage(storage: Option<&str>) -> UpdateAppRequest {
        UpdateAppRequest {
            user_id: "u1".into(),
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: storage.map(|s| ResourceLimits {
                cpu: None,
                memory: None,
                storage: Some(s.to_string()),
                ephemeral_storage: None,
            }),
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        }
    }

    /// update 带 resources.storage → resize_app_storage 收到扩容目标，update 整体成功。
    #[tokio::test]
    pub(crate) async fn update_app_storage_resize_triggered() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-resize").await;
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        service
            .update_app("app-resize", update_request_with_storage(Some("200Gi")))
            .await
            .expect("update with storage");

        assert_eq!(
            runtime.resize_calls.get("app-resize").map(|c| c.clone()),
            Some(vec!["200Gi".to_string()]),
            "resize target forwarded"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before + 1,
            "patch_deployment still applied after successful resize"
        );
    }

    /// 缩容拒绝（ShrinkRejected）→ update 整体 400 Validation，且 patch 不再执行
    /// （resize 在 patch 之前——阻断顺序防"语义错误却滚动生效"）。
    #[tokio::test]
    pub(crate) async fn update_app_storage_shrink_rejected_blocks_update() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-shrink").await;
        *runtime.resize_outcome.lock().expect("outcome lock") =
            Some(StorageResizeOutcome::ShrinkRejected {
                current: "200Gi".into(),
                requested: "50Gi".into(),
            });
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        let error = service
            .update_app("app-shrink", update_request_with_storage(Some("50Gi")))
            .await
            .expect_err("shrink must be rejected");
        assert!(
            matches!(error, AppOperationError::Validation(_)),
            "got: {error}"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before,
            "patch_deployment must NOT run when resize rejected"
        );
    }

    /// resize 后端失败 → update 整体失败（storage 字段承诺生效，不静默降级）。
    #[tokio::test]
    pub(crate) async fn update_app_storage_resize_failure_blocks_update() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-rfail").await;
        runtime.resize_fails.store(true, Ordering::SeqCst);
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        let error = service
            .update_app("app-rfail", update_request_with_storage(Some("200Gi")))
            .await
            .expect_err("resize failure must block update");
        assert!(
            matches!(error, AppOperationError::Backend(_)),
            "got: {error}"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before,
            "patch_deployment must NOT run when resize failed"
        );
    }

    /// update 不带 resources.storage（None 或无 storage 字段）→ resize 不触发。
    #[tokio::test]
    pub(crate) async fn update_app_without_storage_skips_resize() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-nosize").await;

        service
            .update_app("app-nosize", update_request_with_storage(None))
            .await
            .expect("update without storage");

        assert!(
            runtime.resize_calls.is_empty(),
            "resize must not be called without storage"
        );
    }

    /// query_apps 分页校验：page<1 / page_size∉[1,100] → 400（对齐 query_storage 与
    /// publish tasks 口径；此前静默 clamp，超大 page 在 debug 构建乘法溢出 panic）。
    #[tokio::test]
    pub(crate) async fn query_apps_rejects_invalid_pagination_and_sort() {
        let service = test_service(
            tempfile::tempdir().expect("tempdir").path(),
            Arc::new(MockRuntime::default()),
        );
        for (page, page_size) in [(0u32, 20u32), (1, 0), (1, 101)] {
            let request = QueryAppsRequest {
                page: Some(page),
                page_size: Some(page_size),
                ..QueryAppsRequest::default()
            };
            let error = service
                .query_apps(request)
                .await
                .expect_err("invalid pagination must 400");
            assert!(
                matches!(error, AppOperationError::Validation(_)),
                "page={page} page_size={page_size}: {error}"
            );
        }
        let request = QueryAppsRequest {
            sort_by: Some("bogus".into()),
            ..QueryAppsRequest::default()
        };
        let error = service
            .query_apps(request)
            .await
            .expect_err("invalid sort_by must 400");
        assert!(matches!(error, AppOperationError::Validation(_)));
    }

    /// 三档删除语义：delete(purge=true) 销毁存储但**保留**元数据行（误删找回）；
    /// 仅独立 storage/destroy 接口删行。
    #[tokio::test]
    pub(crate) async fn delete_app_purge_keeps_metadata_row_until_explicit_destroy() {
        use crate::test_support::InMemoryMetadataPersistence;
        use shared_types::AppMetadataPersistence as _;

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let persistence = InMemoryMetadataPersistence::new(vec![]);
        let service = test_service(root.path(), runtime);
        service.set_metadata_persistence(persistence.clone());
        let app_dir = root.path().join("app-purge");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let mut create = create_request("app-purge");
        create.name = "keep-me".into();
        service.create_app(create).await.expect("create app");
        assert!(service.metadata.lookup("app-purge").is_some());

        service
            .delete_app("app-purge", true, None)
            .await
            .expect("purge delete");
        assert!(
            service.metadata.lookup("app-purge").is_some(),
            "purge must retain metadata row (three-tier contract)"
        );
        assert!(
            persistence
                .load_all()
                .await
                .expect("persisted")
                .iter()
                .any(|r| r.app_id == "app-purge"),
            "PG row retained after purge"
        );

        service
            .destroy_app_storage(
                shared_types::UserappStage::Prod,
                "app-purge",
                "u-purge",
                "app-purge",
            )
            .await
            .expect("explicit destroy");
        assert!(
            service.metadata.lookup("app-purge").is_none(),
            "explicit storage destroy deletes metadata row"
        );
    }

    /// query_apps 的 name/created_at 过滤:纯内存模式（无 metadata 持久化）维持忽略
    /// （全量返回,旧行为）;注入持久化（PG 模式同构）后经内存 join 生效。
    #[tokio::test]
    pub(crate) async fn query_apps_name_filter_respects_metadata_mode() {
        use crate::test_support::InMemoryMetadataPersistence;
        use container_runtime_api::DeploymentStatus;
        use shared_types::{AppMetadataPersistence as _, AppMetadataRecord};

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        for app_id in ["app-alpha", "app-beta"] {
            runtime.deployments.insert(
                app_id.into(),
                DeploymentStatus {
                    app_id: app_id.into(),
                    ..Default::default()
                },
            );
        }
        let service = test_service(root.path(), runtime.clone());
        // owner 过滤前置：两 app 注册 owner=u1（内存 metadata）
        for app_id in ["app-alpha", "app-beta"] {
            service
                .metadata
                .record(app_id, None, Some("u1".into()), None, None)
                .await;
        }

        let by_name = |name: &str| QueryAppsRequest {
            user_id: "u1".into(),
            page: None,
            page_size: None,
            filters: Some(AppFilters {
                status: None,
                name: Some(name.into()),
                app_ids: None,
                created_at: None,
            }),
            sort_by: None,
            sort_order: None,
        };

        // 纯内存模式:name 过滤忽略（warn）,全量返回
        let response = service.query_apps(by_name("alpha")).await.expect("query");
        assert_eq!(response.items.len(), 2, "memory mode ignores name filter");

        // 注入持久化 + 元数据:过滤生效
        let persistence = InMemoryMetadataPersistence::new(vec![
            AppMetadataRecord {
                app_id: "app-alpha".into(),
                name: Some("alpha".into()),
                user_id: Some("u1".into()),
                tenant_id: None,
                space_id: None,
                created_at: chrono::Utc::now() - chrono::Duration::hours(2),
            },
            AppMetadataRecord {
                app_id: "app-beta".into(),
                name: Some("beta".into()),
                user_id: Some("u1".into()),
                tenant_id: None,
                space_id: None,
                created_at: chrono::Utc::now(),
            },
        ]);
        service.set_metadata_persistence(persistence.clone());
        service.apply_metadata_loaded(persistence.load_all().await.expect("load"));

        let response = service.query_apps(by_name("alpha")).await.expect("query");
        assert_eq!(response.items.len(), 1, "name filter now effective");
        assert_eq!(response.items[0].app_id, "app-alpha");

        // created_at range:只含 2 小时前创建的 alpha
        let now = chrono::Utc::now();
        let response = service
            .query_apps(QueryAppsRequest {
                user_id: "u1".into(),
                page: None,
                page_size: None,
                filters: Some(AppFilters {
                    status: None,
                    name: None,
                    app_ids: None,
                    created_at: Some(DateRange {
                        start: (now - chrono::Duration::hours(3)).to_rfc3339(),
                        end: (now - chrono::Duration::hours(1)).to_rfc3339(),
                    }),
                }),
                sort_by: None,
                sort_order: None,
            })
            .await
            .expect("query by range");
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].app_id, "app-alpha");
    }
