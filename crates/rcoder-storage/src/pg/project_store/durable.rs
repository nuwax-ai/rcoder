//! durable 直写：会话创建的结构性 op 同步事务提交（chat 返回即落库契约）。
//!
//! 与 write-behind（writer.rs，高频幂等 op 的吞吐路径）互补：低频高价值的
//! 结构性写在调用点同步提交，超时/失败降级回队列（HA 语义不变）。
//! op 集与降级路径共用 [`structural_ops_for_insert`]（单一构造点，零漂移）。

use std::sync::Arc;
use std::time::Duration;

use shared_types::ProjectAndContainerInfo;

use super::persist_ops::structural_ops_for_insert;
use super::writer::execute_op;
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
}
