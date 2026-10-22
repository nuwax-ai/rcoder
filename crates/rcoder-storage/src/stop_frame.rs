//! A Stop keeps its pre-RPC registration and a detached physical observation.
use shared_types::ProjectAndContainerInfo;
use std::sync::Arc;

#[derive(Clone)]
pub struct ProjectStopFrame {
    pub(crate) local_version: Arc<ProjectAndContainerInfo>,
    frozen: ProjectAndContainerInfo,
}

impl ProjectStopFrame {
    pub(crate) fn capture(local_version: Arc<ProjectAndContainerInfo>) -> Self {
        let mut frozen = (*local_version).clone();
        frozen.set_container(local_version.container_info());
        Self {
            local_version,
            frozen,
        }
    }

    pub fn project(&self) -> &ProjectAndContainerInfo {
        &self.frozen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectAdapter, ProjectStoreBackend};
    use shared_types::{
        ContainerBasicInfo, ProjectStore, ServiceType, persistence::PersistenceWriteOutcome,
    };

    fn info() -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new("memory-stop-frame".into());
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        info.set_user_id(Some("memory-stop-owner".into()));
        info.set_container(Some(ContainerBasicInfo {
            container_id: "memory-stop-pod".into(),
            container_name: "memory-stop-container".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 8086,
            external_port: 0,
            project_id: "memory-stop-owner".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://memory-stop-container".into(),
            workload_uid: Some("memory-stop-workload".into()),
        }));
        info.add_session("same-sdk-sid");
        info
    }

    #[tokio::test]
    async fn memory_captured_stop_preserves_a_new_chat_with_none_request_and_zero_revision() {
        let adapter = ProjectAdapter::new("test".into(), "cluster.local".into());
        let backend = ProjectStoreBackend::Memory(Arc::new(adapter));
        let original = info();
        backend
            .insert(original.project_id().into(), Arc::new(original))
            .unwrap();
        let frame = backend
            .capture_project_stop_registration("memory-stop-frame")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.project().persistence_identity().revision, 0);
        assert!(frame.project().request_id().is_none());
        let mut newer = (*backend.get("memory-stop-frame").unwrap()).clone();
        newer.set_request_id(None);
        newer.add_session("fresh-sdk-sid");
        backend
            .insert("memory-stop-frame".into(), Arc::new(newer))
            .unwrap();
        assert_eq!(
            backend
                .clear_sessions_if_current_durable(&frame)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Superseded
        );
        assert_eq!(
            backend.get("memory-stop-frame").unwrap().latest_session(),
            Some("fresh-sdk-sid")
        );
        let current = backend
            .capture_project_stop_registration("memory-stop-frame")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            backend
                .clear_sessions_if_current_durable(&current)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
        assert!(
            backend
                .get("memory-stop-frame")
                .unwrap()
                .sessions()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn memory_stop_frame_keeps_its_detached_original_physical_identity() {
        let adapter = ProjectAdapter::new("test".into(), "cluster.local".into());
        let backend = ProjectStoreBackend::Memory(Arc::new(adapter));
        let original = info();
        backend
            .insert(original.project_id().into(), Arc::new(original))
            .unwrap();
        let frame = backend
            .capture_project_stop_registration("memory-stop-frame")
            .await
            .unwrap()
            .unwrap();
        let mut replacement = (*backend.get("memory-stop-frame").unwrap()).clone();
        let mut physical = replacement.container_info().unwrap();
        physical.container_id = "replacement-pod".into();
        replacement.set_container(Some(physical));
        backend
            .insert("memory-stop-frame".into(), Arc::new(replacement))
            .unwrap();
        assert_eq!(
            frame.project().container_info().unwrap().container_id,
            "memory-stop-pod"
        );
        assert_eq!(
            backend
                .clear_sessions_if_current_durable(&frame)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Superseded
        );
        assert!(
            backend
                .get("memory-stop-frame")
                .unwrap()
                .sessions()
                .contains("same-sdk-sid")
        );
    }
}
