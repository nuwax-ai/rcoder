//! PgStore 的 ProjectStore 契约实现（从 pg/mod.rs 拆出，extension-impl）
//!
//! 读写全转发内层内存镜像；变更方法成功后 enqueue 持久化 op
//! （结构性 op 直接入队，Touch/UpdateAgentStatus 经节流）。

use std::sync::Arc;

use shared_types::{
    ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore, ServiceType, StorageStats,
};

use super::PgStore;
use super::persist_ops::PersistOp;
use crate::adapter::{code_to_agent_status, container_entry_key};

impl ProjectStore for PgStore {
    fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.inner.get(project_id)
    }

    fn contains_key(&self, project_id: &str) -> bool {
        self.inner.contains_key(project_id)
    }

    fn iter(&self) -> Vec<(String, Arc<ProjectAndContainerInfo>)> {
        self.inner.iter()
    }

    fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.inner.get_by_session_id(session_id)
    }

    fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        self.inner.get_container_name_by_session(session_id)
    }

    fn get_all_container_records(&self) -> Vec<ContainerBasicInfo> {
        self.inner.get_all_container_records()
    }

    fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.inner.get_projects_by_container_id(container_id)
    }

    fn get_container_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Option<ContainerBasicInfo> {
        self.inner.get_container_by_user_id(user_id, service_type)
    }

    fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        self.inner.get_container_by_pod_id(pod_id)
    }

    fn find_projects_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.inner.find_projects_by_user_id(user_id, service_type)
    }

    fn find_projects_by_pod_id(&self, pod_id: &str) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.inner.find_projects_by_pod_id(pod_id)
    }

    fn get_stats(&self) -> StorageStats {
        self.inner.get_stats()
    }

    fn dump_summary(&self) -> String {
        format!("{} [pg]", self.inner.dump_summary())
    }

    fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) -> anyhow::Result<()> {
        self.inner.insert(project_id, Arc::clone(&info))?;
        self.persist_upsert(&info)
    }

    fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner
            .insert_with_session(project_id, Arc::clone(&info), session_id)?;
        // op 集单一构造点（与 durable 直写/降级路径同源）
        if let Some(sid) = session_id {
            for op in super::persist_ops::structural_ops_for_insert(&info, sid)? {
                self.enqueue_structural(op);
            }
        } else {
            self.persist_upsert(&info)?;
        }
        Ok(())
    }

    fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        if self.inner.add_session_to_project(project_id, session_id) {
            let container_name = self
                .inner
                .get(project_id)
                .map(|info| container_entry_key(&info));
            self.enqueue_structural(PersistOp::AddSession {
                project_id: project_id.to_string(),
                session_id: session_id.to_string(),
                container_name,
            });
            self.enqueue_throttled(
                &format!("p:{project_id}"),
                PersistOp::TouchProject {
                    project_id: project_id.to_string(),
                    last_activity: chrono::Utc::now(),
                },
            );
            true
        } else {
            false
        }
    }

    fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let removed = self.inner.remove(project_id);
        if removed.is_some() {
            self.touch_throttled.invalidate(&format!("p:{project_id}"));
            self.enqueue_structural(PersistOp::RemoveProject {
                project_id: project_id.to_string(),
            });
        }
        removed
    }

    fn clear_session(&self, project_id: &str) {
        self.inner.clear_session(project_id);
        self.enqueue_structural(PersistOp::ClearSessions {
            project_id: project_id.to_string(),
        });
    }

    fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        if self.inner.clear_session_one(project_id, session_id) {
            self.touch_throttled.invalidate(&format!("s:{session_id}"));
            self.enqueue_structural(PersistOp::RemoveSession {
                session_id: session_id.to_string(),
            });
            true
        } else {
            false
        }
    }

    fn update_activity(&self, project_id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        let result = self.inner.update_activity(project_id);
        if let Some(at) = result {
            self.enqueue_throttled(
                &format!("p:{project_id}"),
                PersistOp::TouchProject {
                    project_id: project_id.to_string(),
                    last_activity: at,
                },
            );
            if let Some(name) = self
                .inner
                .get(project_id)
                .map(|info| container_entry_key(&info))
            {
                self.enqueue_throttled(
                    &format!("c:{name}"),
                    PersistOp::TouchContainer {
                        container_name: name,
                        last_activity: at,
                    },
                );
            }
        }
        result
    }

    fn update_session_activity(&self, session_id: &str) -> bool {
        if self.inner.update_session_activity(session_id) {
            self.enqueue_throttled(
                &format!("s:{session_id}"),
                PersistOp::TouchSession {
                    session_id: session_id.to_string(),
                    last_seen_at: chrono::Utc::now(),
                },
            );
            true
        } else {
            false
        }
    }

    fn update_agent_status(&self, project_id: &str, status: i32, message: &str) -> bool {
        if self.inner.update_agent_status(project_id, status, message) {
            let agent_status = code_to_agent_status(status, message);
            match serde_json::to_value(agent_status) {
                Ok(value) => {
                    self.enqueue_throttled(
                        &format!("a:{project_id}"),
                        PersistOp::UpdateAgentStatus {
                            project_id: project_id.to_string(),
                            agent_status: value,
                        },
                    );
                }
                // 序列化几乎不会失败，一旦发生必须可见：内存已更新而 PG 永久丢失
                Err(e) => {
                    tracing::warn!(
                        "[STORAGE_PG] agent_status serialize failed (not persisted): project_id={}, status={}, error={e}",
                        project_id,
                        status
                    );
                }
            }
            true
        } else {
            false
        }
    }

    fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize) {
        let result = self.inner.delete_container_with_projects(container_id);
        if result.0 {
            self.enqueue_structural(PersistOp::DeleteContainerWithProjects {
                container_id: container_id.to_string(),
            });
        }
        result
    }
}
