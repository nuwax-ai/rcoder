//! durable 直写：会话创建的结构性 op 同步事务提交（chat 返回即落库契约）。
//!
//! 与 write-behind（writer.rs，高频幂等 op 的吞吐路径）互补：低频高价值的
//! 结构性写在调用点同步提交，超时/失败降级回队列（HA 语义不变）。

use std::sync::Arc;
use std::time::Duration;

use shared_types::ProjectAndContainerInfo;

use super::PgStore;
use super::container_entry_key;
use super::persist_ops::{ContainerSnapshot, PersistOp, ProjectSnapshot};
use super::repo;

impl PgStore {
    /// 结构性写的事务直写超时：正常毫秒级完成；超时降级走 write-behind。
    const DURABLE_COMMIT_TIMEOUT: Duration = Duration::from_millis(600);

    /// 会话创建的结构性 op **事务直写**（durable 路径）：
    /// 内存镜像更新 + container/project/session 于同一 sqlx 事务提交——
    /// 方法返回（Ok）即 PG 主库已提交，chat 侧据此保证"session_id 交到
    /// 前端手上时任何副本回源直查必命中"。
    ///
    /// 提交超时/失败降级：事务丢弃（drop=rollback），改入 write-behind
    /// 队列（现有异步路径）——chat 不失败（内存真源），可见性窗口仅在
    /// PG 故障态退化。降级路径 op 会入队，成功路径不入队（无双写）。
    pub async fn insert_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        // 1. 内存镜像（与 insert_with_session 的内存部分一致）
        self.inner
            .insert_with_session(project_id, Arc::clone(&info), Some(session_id))?;

        // 2. 事务直写（与 persist_upsert 相同的 op 集：容器 + 项目 + 会话）
        let session_project = info.project_id().to_string();
        let container_name = info.container_info().map(|_| container_entry_key(&info));
        let snapshot = ProjectSnapshot::from_info(&info)?;
        let durable = async {
            let mut tx = self.pool.begin().await?;
            if let Some(basic) = info.container_info()
                && let Some(st) = info.service_type()
            {
                repo::upsert_container(
                    &mut *tx,
                    &ContainerSnapshot::from_info(&container_entry_key(&info), &basic, &st),
                )
                .await?;
            }
            repo::upsert_project(&mut *tx, &snapshot).await?;
            repo::add_session(
                &mut *tx,
                &session_project,
                session_id,
                container_name.as_deref(),
            )
            .await?;
            tx.commit().await
        };
        match tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, durable).await {
            Ok(Ok(())) => {
                tracing::debug!(
                    "[STORAGE_PG] durable commit ok: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                Ok(())
            }
            outcome => {
                let reason = match outcome {
                    Ok(Ok(())) => unreachable!("covered by first arm"),
                    Ok(Err(e)) => format!("sql error: {e}"),
                    Err(_) => "timeout".to_string(),
                };
                tracing::warn!(
                    "[STORAGE_PG] durable commit failed ({reason}), falling back to write-behind: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                // 降级：与 insert_with_session 的入队路径完全一致（幂等，writer 重放安全）
                tracing::warn!(
                    "[STORAGE_PG] durable commit failed, falling back to write-behind: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                self.persist_upsert(&info)?;
                self.enqueue_structural(PersistOp::AddSession {
                    project_id: session_project,
                    session_id: session_id.to_string(),
                    container_name,
                });
                Ok(())
            }
        }
    }
}
