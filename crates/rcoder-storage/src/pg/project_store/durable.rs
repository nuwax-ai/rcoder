//! durable 直写：会话创建的结构性 op 同步事务提交（chat 返回即落库契约）。
//!
//! 与 write-behind（writer.rs，高频幂等 op 的吞吐路径）互补：低频高价值的
//! 结构性写在调用点同步提交，超时/失败降级回队列（HA 语义不变）。
//! op 集与降级路径共用 [`structural_ops_for_insert`]（单一构造点，零漂移）。

use std::sync::Arc;
use std::time::Duration;

use shared_types::ProjectAndContainerInfo;

use super::persist_ops::{PersistOp, structural_ops_for_insert};
use super::writer::execute_op;
use crate::adapter::container_entry_key;
use crate::pg::PgStore;

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

        // 2. 事务直写（op 集单一构造点，复用 writer 的 execute_op 执行）
        let session_project = info.project_id().to_string();
        let ops = structural_ops_for_insert(&info, session_id)?;
        let durable = async {
            let result: anyhow::Result<()> = async {
                let mut tx = self.pool.begin().await?;
                for op in &ops {
                    execute_op(&mut tx, op).await?;
                }
                tx.commit().await?;
                Ok(())
            }
            .await;
            result
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
                // 降级：op 集与成功路径同源（幂等，writer 重放安全；
                // upsert 的 last_activity 防回退守卫拦住旧快照覆盖）
                for op in ops {
                    self.enqueue_structural(op);
                }
                Ok(())
            }
        }
    }

    /// 追加 session 的 durable 变体（/chat 域响应后映射补录）：
    /// 内存 add + AddSession 单条事务直写，超时/失败降级 write-behind
    /// （降级后由队列按序重放——project 行若尚未 flush 会在队列中先于
    /// AddSession 执行，FK 依赖最终成立）。
    ///
    /// 返回 `Ok(false)` 表示 project 不存在（并发删除，内存 add 未发生）。
    pub async fn add_session_durable(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        // 1. 内存镜像（与 add_session_to_project 的内存部分一致）
        if !self.inner.add_session_to_project(project_id, session_id) {
            return Ok(false);
        }
        let container_name = self
            .inner
            .get(project_id)
            .map(|info| container_entry_key(&info));

        // 2. 单条事务直写
        let op = PersistOp::AddSession {
            project_id: project_id.to_string(),
            session_id: session_id.to_string(),
            container_name,
        };
        self.execute_structural_durable(op, "add_session").await;
        Ok(true)
    }

    /// 结构性删除的 durable 变体：内存删 + 单条事务直写。
    ///
    /// 消除 durable 直写与 write-behind 队列的**倒挂窗口**：删除走队列时，
    /// 时序可为 remove 入队 → durable insert 提交返回 → writer 重放积压的
    /// RemoveProject 把 durable 刚提交的行删掉（破坏"返回即落库"跨副本可见
    /// 性契约）。删除类与插入类同走同步事务后，PG 行级锁串行化天然保序——
    /// 最终态取决于真实调用顺序。低频路径（stop/清理），同步代价可忽略。
    pub async fn remove_durable(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let removed = self.inner.remove(project_id)?;
        self.touch_throttled.invalidate(&format!("p:{project_id}"));
        self.execute_structural_durable(
            PersistOp::RemoveProject {
                project_id: project_id.to_string(),
            },
            "remove",
        )
        .await;
        Some(removed)
    }

    /// [`Self::remove_durable`] 的 clear_session 同构（stop 场景清全部会话）。
    pub async fn clear_session_durable(&self, project_id: &str) {
        self.inner.clear_session(project_id);
        self.execute_structural_durable(
            PersistOp::ClearSessions {
                project_id: project_id.to_string(),
            },
            "clear_sessions",
        )
        .await;
    }

    /// [`Self::remove_durable`] 的单 session 删除同构。
    pub async fn clear_session_one_durable(&self, project_id: &str, session_id: &str) -> bool {
        if !self.inner.clear_session_one(project_id, session_id) {
            return false;
        }
        self.touch_throttled.invalidate(&format!("s:{session_id}"));
        self.execute_structural_durable(
            PersistOp::RemoveSession {
                session_id: session_id.to_string(),
            },
            "remove_session",
        )
        .await;
        true
    }

    /// [`Self::remove_durable`] 的容器级删除同构（容器销毁路径）。
    pub async fn delete_container_with_projects_durable(
        &self,
        container_id: &str,
    ) -> (bool, usize) {
        let result = self.inner.delete_container_with_projects(container_id);
        if result.0 {
            self.execute_structural_durable(
                PersistOp::DeleteContainerWithProjects {
                    container_id: container_id.to_string(),
                },
                "delete_container",
            )
            .await;
        }
        result
    }

    /// 单条结构性 op 的事务直写（600ms 超时降级 write-behind 队列）。
    /// op 已在内存侧生效——降级入队由 writer 幂等重放兜底。
    async fn execute_structural_durable(&self, op: PersistOp, name: &str) {
        let durable = async {
            let result: anyhow::Result<()> = async {
                let mut tx = self.pool.begin().await?;
                execute_op(&mut tx, &op).await?;
                tx.commit().await?;
                Ok(())
            }
            .await;
            result
        };
        match tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, durable).await {
            Ok(Ok(())) => {
                // 只打 op.kind()：op 全量 Debug 会含 ProjectSnapshot 的
                // model_provider（明文 api_key），防止日后接入 UpsertProject 泄密
                tracing::debug!("[STORAGE_PG] durable {name} ok: op={}", op.kind());
            }
            outcome => {
                let reason = match outcome {
                    Ok(Ok(())) => unreachable!("covered by first arm"),
                    Ok(Err(e)) => format!("sql error: {e}"),
                    Err(_) => "timeout".to_string(),
                };
                tracing::warn!(
                    "[STORAGE_PG] durable {name} failed ({reason}), falling back to write-behind: op={}",
                    op.kind()
                );
                self.enqueue_structural(op);
            }
        }
    }
}
