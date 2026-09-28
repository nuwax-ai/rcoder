use std::sync::Arc;

use super::*;
use crate::test_support::{MockRuntime, test_service};

/// dev query 依赖 locator 做 builder 在跑探测——stub 恒"在"（非 orphan）
struct StubDevLocator;
#[async_trait::async_trait]
impl shared_types::UserappDevLocator for StubDevLocator {
    async fn dev_file_server_addr(&self, _app_id: &str) -> Result<String, String> {
        Ok("http://127.0.0.1:60000".to_string())
    }
    async fn dev_container_alive(&self, _app_id: &str) -> Result<bool, String> {
        Ok(true)
    }
}

/// storage 的 app_stage 分派落点：workspace_volume_name / list_workspace_identifiers
/// 必须按 app_stage 换 ServiceType（dev→UserappBuilder / prod→Userapp）——K8s 卷
/// label 与 Docker 目录树都按它分形，分派错即查错卷。
#[tokio::test]
async fn storage_env_dispatches_service_type() {
    let runtime = Arc::new(MockRuntime::default());
    let root = tempfile::tempdir().expect("test root");
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    for app in ["app1", "appdev", "appprod"] {
        service
            .metadata
            .record(app, None, None, None)
            .await
            .expect("owner");
    }

    service
        .metadata
        .record("app1", None, None, None)
        .await
        .expect("register owner");
    service
        .get_app_storage(UserappStage::Prod, "app1")
        .await
        .expect("prod storage");
    service
        .get_app_storage(UserappStage::Dev, "app1")
        .await
        .expect("dev storage");

    let calls = runtime.volume_name_calls.get("app1").expect("calls");
    assert_eq!(
        *calls,
        vec!["Userapp".to_string(), "UserappBuilder".to_string()],
        "prod 先查运行卷、dev 查开发卷（ServiceType 分派）"
    );
}

/// query 的 app_stage 分派：dev 清单枚举 UserappBuilder 卷（不并入 Deployment 集），
/// prod 枚举 Userapp 卷（并入运行中 app 兜底）。
#[tokio::test]
async fn query_storage_env_selects_volume_family() {
    let runtime = Arc::new(MockRuntime::default());
    runtime
        .workspace_ids
        .insert("UserappBuilder".to_string(), vec!["appdev".to_string()]);
    runtime
        .workspace_ids
        .insert("Userapp".to_string(), vec!["appprod".to_string()]);
    let root = tempfile::tempdir().expect("test root");
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .set_dev_locator(Arc::new(StubDevLocator))
        .expect("locator");
    for app in ["app1", "appdev", "appprod"] {
        service
            .metadata
            .record(app, None, None, None)
            .await
            .expect("owner");
    }
    *service.dev_locator.write().expect("dev_locator lock") = Some(Arc::new(StubDevLocator));

    let dev_resp = service
        .query_storage(
            UserappStage::Dev,
            QueryStorageRequest {
                page: 1,
                page_size: 10,
                filters: None,
            },
        )
        .await
        .expect("dev query");
    assert_eq!(
        dev_resp
            .items
            .iter()
            .map(|i| i.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["appdev"],
        "dev 清单只含开发卷"
    );

    let prod_resp = service
        .query_storage(
            UserappStage::Prod,
            QueryStorageRequest {
                page: 1,
                page_size: 10,
                filters: None,
            },
        )
        .await
        .expect("prod query");
    assert_eq!(
        prod_resp
            .items
            .iter()
            .map(|i| i.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["appprod"],
        "prod 清单只含运行卷"
    );
}
