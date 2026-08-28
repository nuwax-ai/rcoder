//! ProjectAdapter 测试（从 adapter/tests.rs 单文件按主题拆分）。
//!
//! - [`crud_session_tests`]：CRUD / session 操作 / RAII 单线程族
//! - [`concurrency_tests`]：并发插入删除 / 共享容器回收压力族
//! - [`index_lookup_tests`]：user_id / pod_id 索引与查找一致性族
//!
//! 公共 fixture（make_adapter / create_test_info / join_with_timeout 等）在本文件。

mod concurrency_tests;
mod crud_session_tests;
mod index_lookup_tests;

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use shared_types::{ContainerBasicInfo, ProjectExtendedFields, ServiceType};

/// 测试用的 K8s namespace
const TEST_NAMESPACE: &str = "test-namespace";
/// 测试用的 K8s 集群域名
const TEST_CLUSTER_DOMAIN: &str = "test.cluster.local";

fn create_test_info(project_id: &str) -> ProjectAndContainerInfo {
    let mut info = ProjectAndContainerInfo::new(project_id.to_string());
    info.set_service_type(Some(ServiceType::WebAgentRunner));
    info
}

fn create_test_info_with_container(
    project_id: &str,
    container_name: &str,
) -> ProjectAndContainerInfo {
    let mut info = create_test_info(project_id);
    info.set_container(Some(ContainerBasicInfo {
        container_id: format!("{}-id", container_name),
        container_name: container_name.to_string(),
        container_ip: "127.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: project_id.to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{}", container_name),
    }));
    info
}

fn make_adapter() -> ProjectAdapter {
    let (adapter, _) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    adapter
}

fn join_with_timeout<T>(handle: thread::JoinHandle<T>, timeout_secs: u64) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

fn drain_cleanup_requests(
    rx: &Mutex<tokio::sync::mpsc::Receiver<CleanupRequest>>,
) -> Vec<CleanupRequest> {
    let mut guard = rx.lock().unwrap();
    let mut requests = vec![];
    while let Ok(req) = guard.try_recv() {
        requests.push(req);
    }
    requests
}

fn create_shared_project(
    project_id: &str,
    user_id: &str,
    container: &ContainerBasicInfo,
) -> ProjectAndContainerInfo {
    let mut info = ProjectAndContainerInfo::from_parts(
        project_id.to_string(),
        Some(user_id.to_string()),
        None,
        None,
        Some(container.clone()),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info
}
