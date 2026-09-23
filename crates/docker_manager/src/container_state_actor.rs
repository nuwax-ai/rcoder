//! Container State Actor
//!
//! 使用 Actor 模式管理容器状态，避免 DashMap 跨 await 持有锁导致的死锁问题。
//!
//! # 架构
//! - `ContainerStateActor`: 独占 HashMap，在独立 task 中运行，处理所有状态操作
//! - `ContainerStateHandle`: 可克隆的句柄，提供 async 方法与 Actor 通信
//!
//! # 优点
//! - 完全无锁，不会死锁
//! - 状态变更顺序化，更易调试
//! - 符合 Rust async 最佳实践

use crate::DockerContainerInfo;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

/// Actor 通道缓冲区大小
const CHANNEL_BUFFER_SIZE: usize = 256;

/// 容器状态操作命令
#[derive(Debug)]
pub enum ContainerStateCommand {
    /// 获取容器信息
    Get {
        key: String,
        reply: oneshot::Sender<Option<DockerContainerInfo>>,
    },
    /// 插入/更新容器信息
    Insert {
        key: String,
        info: DockerContainerInfo,
    },
    /// 移除容器信息
    Remove {
        key: String,
        reply: oneshot::Sender<Option<DockerContainerInfo>>,
    },
    /// 获取所有容器列表
    List {
        reply: oneshot::Sender<Vec<DockerContainerInfo>>,
    },
    /// 获取所有 key 列表
    Keys { reply: oneshot::Sender<Vec<String>> },
    /// 获取容器数量
    Len { reply: oneshot::Sender<usize> },
    /// 检查 key 是否存在
    Contains {
        key: String,
        reply: oneshot::Sender<bool>,
    },
    /// 通过回调更新容器信息（用于原地更新）
    UpdateWith {
        key: String,
        /// 更新后的信息（如果 key 存在）
        updated_info: DockerContainerInfo,
        reply: oneshot::Sender<bool>,
    },
    /// Apply an inspect result only to the physical container that was queried.
    UpdateIfContainerId {
        key: String,
        container_id: String,
        updated_info: DockerContainerInfo,
        reply: oneshot::Sender<bool>,
    },
    /// 条件移除：只有当 container_id 匹配时才移除
    RemoveIfContainerId {
        key: String,
        container_id: String,
        reply: oneshot::Sender<Option<DockerContainerInfo>>,
    },
    /// 移除所有 container_id 匹配的条目（单次遍历，返回被移除项）。
    /// 用于 cleanup_all_containers 等场景，替代逐个 list() 的 O(n²)；仍按
    /// container_id 精确匹配，保留"防误删重启新容器"语义。
    RemoveAllByContainerId {
        container_id: String,
        reply: oneshot::Sender<Vec<DockerContainerInfo>>,
    },
}

/// 容器状态 Actor
///
/// 独占 HashMap，在独立 task 中运行
pub struct ContainerStateActor {
    containers: HashMap<String, DockerContainerInfo>,
    receiver: mpsc::Receiver<ContainerStateCommand>,
    /// 与 handle 共享的状态代次：仅在实际应用变更**之后**递增——
    /// 读侧宁可多失效一次，绝不把未落盘的变更标成已可见
    list_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ContainerStateActor {
    /// 创建新的 Actor 和 Handle
    pub fn new() -> (Self, ContainerStateHandle) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let list_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let actor = Self {
            containers: HashMap::new(),
            receiver,
            list_epoch: list_epoch.clone(),
        };
        let handle = ContainerStateHandle { sender, list_epoch };
        (actor, handle)
    }

    /// 变更应用后递增共享代次。
    fn bump_epoch(&self) {
        self.list_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// 运行 Actor 事件循环
    ///
    /// 这个方法应该在 `tokio::spawn` 中调用
    pub async fn run(mut self) {
        debug!("[ACTOR] ContainerStateActor started");

        while let Some(cmd) = self.receiver.recv().await {
            // panic 隔离（对齐 sse_stream / agent_abstraction 的 catch_unwind 先例）：
            // actor 是无人监督的常驻任务，panic 静默消亡会让 DockerManager 的内存
            // 容器表永久读空（所有 handle 调用降级为空值且无自愈）。单命令 panic
            // 只跳过该命令继续服务 —— 单条 HashMap 操作不存在跨命令半完成状态
            if let Err(_panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.handle_command(cmd);
            })) {
                error!("[ACTOR] handle_command panicked, command skipped (actor keeps running)");
            }
        }

        debug!("[ACTOR] ContainerStateActor stopped (all senders dropped)");
    }

    /// 处理单个命令
    fn handle_command(&mut self, cmd: ContainerStateCommand) {
        match cmd {
            ContainerStateCommand::Get { key, reply } => {
                let result = self.containers.get(&key).cloned();
                if reply.send(result).is_err() {
                    warn!("[ACTOR] Get reply channel closed");
                }
            }
            ContainerStateCommand::Insert { key, info } => {
                self.containers.insert(key, info);
                self.bump_epoch();
            }
            ContainerStateCommand::Remove { key, reply } => {
                let result = self.containers.remove(&key);
                self.bump_epoch();
                if reply.send(result).is_err() {
                    warn!("[ACTOR] Remove reply channel closed");
                }
            }
            ContainerStateCommand::List { reply } => {
                let result: Vec<_> = self.containers.values().cloned().collect();
                if reply.send(result).is_err() {
                    warn!("[ACTOR] List reply channel closed");
                }
            }
            ContainerStateCommand::Keys { reply } => {
                let result: Vec<_> = self.containers.keys().cloned().collect();
                if reply.send(result).is_err() {
                    warn!("[ACTOR] Keys reply channel closed");
                }
            }
            ContainerStateCommand::Len { reply } => {
                if reply.send(self.containers.len()).is_err() {
                    warn!("[ACTOR] Len reply channel closed");
                }
            }
            ContainerStateCommand::Contains { key, reply } => {
                if reply.send(self.containers.contains_key(&key)).is_err() {
                    warn!("[ACTOR] Contains reply channel closed");
                }
            }
            ContainerStateCommand::UpdateWith {
                key,
                updated_info,
                reply,
            } => {
                let existed = if let std::collections::hash_map::Entry::Occupied(mut e) =
                    self.containers.entry(key)
                {
                    e.insert(updated_info);
                    true
                } else {
                    false
                };
                self.bump_epoch();
                if reply.send(existed).is_err() {
                    warn!("[ACTOR] UpdateWith reply channel closed");
                }
            }
            ContainerStateCommand::UpdateIfContainerId {
                key,
                container_id,
                updated_info,
                reply,
            } => {
                let updated = match self.containers.get_mut(&key) {
                    Some(current) if current.container_id == container_id => {
                        *current = updated_info;
                        self.bump_epoch();
                        true
                    }
                    _ => false,
                };
                if reply.send(updated).is_err() {
                    warn!("[ACTOR] UpdateIfContainerId reply channel closed");
                }
            }
            ContainerStateCommand::RemoveIfContainerId {
                key,
                container_id,
                reply,
            } => {
                let should_remove = if let Some(info) = self.containers.get(&key) {
                    info.container_id == container_id
                } else {
                    false
                };

                let result = if should_remove {
                    self.containers.remove(&key)
                } else {
                    None
                };
                self.bump_epoch();

                if reply.send(result).is_err() {
                    warn!("[ACTOR] RemoveIfContainerId reply channel closed");
                }
            }
            ContainerStateCommand::RemoveAllByContainerId {
                container_id,
                reply,
            } => {
                // 单次遍历收集匹配 key，再逐个 remove（O(n)，一次 actor 往返）
                let matching_keys: Vec<String> = self
                    .containers
                    .iter()
                    .filter(|(_, info)| info.container_id == container_id)
                    .map(|(k, _)| k.clone())
                    .collect();
                let removed: Vec<DockerContainerInfo> = matching_keys
                    .into_iter()
                    .filter_map(|k| self.containers.remove(&k))
                    .collect();
                self.bump_epoch();
                if reply.send(removed).is_err() {
                    warn!("[ACTOR] RemoveAllByContainerId reply channel closed");
                }
            }
        }
    }
}

/// 容器状态句柄
///
/// 可克隆，用于与 Actor 通信。
///
/// `list_epoch` 在每次状态变更（insert/remove/update）前递增：供
/// `DockerRuntime::list_containers` 的 TTL 缓存做代次校验——新建/删除
/// 容器后列表立即可见，不再受 15s TTL 内的陈旧快照影响（e2e
/// pod/list 对 ensure 后即时可见性的时序要求）。
#[derive(Clone)]
pub struct ContainerStateHandle {
    sender: mpsc::Sender<ContainerStateCommand>,
    list_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ContainerStateHandle {
    /// 当前状态代次（每次实际变更后递增；只读观察用）。
    pub fn list_epoch(&self) -> u64 {
        self.list_epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 获取容器信息
    pub async fn get(&self, key: &str) -> Option<DockerContainerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::Get {
                key: key.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Get command - actor stopped");
            return None;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] Get reply failed - actor task died");
            None
        })
    }

    /// 插入/更新容器信息
    pub async fn insert(&self, key: String, info: DockerContainerInfo) {
        if self
            .sender
            .send(ContainerStateCommand::Insert { key, info })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Insert command - actor stopped");
        }
    }

    /// 移除容器信息
    pub async fn remove(&self, key: &str) -> Option<DockerContainerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::Remove {
                key: key.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Remove command - actor stopped");
            return None;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] Remove reply failed - actor task died");
            None
        })
    }

    /// 获取所有容器列表
    pub async fn list(&self) -> Vec<DockerContainerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::List { reply })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send List command - actor stopped");
            return Vec::new();
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] List reply failed - actor task died");
            Vec::new()
        })
    }

    /// 获取所有 key 列表
    pub async fn keys(&self) -> Vec<String> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::Keys { reply })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Keys command - actor stopped");
            return Vec::new();
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] Keys reply failed - actor task died");
            Vec::new()
        })
    }

    /// 获取容器数量
    pub async fn len(&self) -> usize {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::Len { reply })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Len command - actor stopped");
            return 0;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] Len reply failed - actor task died");
            0
        })
    }

    /// 检查是否为空
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// 检查 key 是否存在
    pub async fn contains_key(&self, key: &str) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::Contains {
                key: key.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send Contains command - actor stopped");
            return false;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] Contains reply failed - actor task died");
            false
        })
    }

    /// 条件更新：如果 key 存在则更新
    ///
    /// 返回 true 表示Update succeeded，false 表示 key 不存在
    pub async fn update_if_exists(&self, key: &str, info: DockerContainerInfo) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::UpdateWith {
                key: key.to_string(),
                updated_info: info,
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send UpdateWith command - actor stopped");
            return false;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] UpdateWith reply failed - actor task died");
            false
        })
    }

    pub async fn update_if_container_id(
        &self,
        key: &str,
        container_id: &str,
        info: DockerContainerInfo,
    ) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::UpdateIfContainerId {
                key: key.to_owned(),
                container_id: container_id.to_owned(),
                updated_info: info,
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send UpdateIfContainerId command - actor stopped");
            return false;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] UpdateIfContainerId reply failed - actor task died");
            false
        })
    }

    /// 条件移除：只有当 container_id 匹配时才移除
    ///
    /// 防止在清理时误删刚重启的容器（CAS 操作）
    pub async fn remove_if_container_id(
        &self,
        key: &str,
        container_id: &str,
    ) -> Option<DockerContainerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::RemoveIfContainerId {
                key: key.to_string(),
                container_id: container_id.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send RemoveIfContainerId command - actor stopped");
            return None;
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] RemoveIfContainerId reply failed - actor task died");
            None
        })
    }

    /// 移除所有 container_id 匹配的条目（单次 actor 往返，替代多次 list() 的 O(n²)）。
    pub async fn remove_all_by_container_id(&self, container_id: &str) -> Vec<DockerContainerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .sender
            .send(ContainerStateCommand::RemoveAllByContainerId {
                container_id: container_id.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            error!("[HANDLE] Failed to send RemoveAllByContainerId command - actor stopped");
            return vec![];
        }
        rx.await.unwrap_or_else(|_| {
            error!("[HANDLE] RemoveAllByContainerId reply failed - actor task died");
            vec![]
        })
    }
}

impl std::fmt::Debug for ContainerStateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerStateHandle")
            .field("sender_closed", &self.sender.is_closed())
            .finish()
    }
}

// 单元测试见下方；跨模块集成验证由 docker_runtime 的 list 缓存代次测试
// 与 e2e pod/list 即时可见性场景承担。

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_info(name: &str) -> DockerContainerInfo {
        serde_json::from_value(serde_json::json!({
            "container_id": format!("id-{name}"),
            "container_name": name,
            "project_id": name,
            "image": "fixture",
            "status": "Running",
            "created_at": "2026-09-16T00:00:00Z",
            "host_path": "/tmp",
            "container_path": "/workspace",
            "port_bindings": {},
            "assigned_port": 0,
            "internal_port": 8086,
            "network_name": "test"
        }))
        .expect("minimal container info")
    }

    #[tokio::test]
    async fn list_epoch_bumps_on_every_state_mutation() {
        // list 缓存代次失效的输入契约：任一状态变更都必须在实际应用后递增
        // epoch——否则创建/删除后的 list_containers 命中陈旧快照（e2e
        // pod/list ensure 后即时可见性，2026-09-16 复现）
        let (actor, handle) = ContainerStateActor::new();
        tokio::spawn(actor.run());
        let base = handle.list_epoch();

        // insert 是发后即忘：用同通道 FIFO 的读命令做屏障（get 返回时
        // insert 必已应用，代次递增已可见——不 sleep、不轮询）
        handle.insert("k1".into(), minimal_info("k1")).await;
        assert!(handle.get("k1").await.is_some(), "insert 已应用");
        let after_insert = handle.list_epoch();
        assert!(after_insert > base, "insert 必须递增代次");

        handle.update_if_exists("k1", minimal_info("k1")).await;
        let after_update = handle.list_epoch();
        assert!(after_update > after_insert, "update 必须递增代次");

        handle.remove("k1").await;
        let after_remove = handle.list_epoch();
        assert!(after_remove > after_update, "remove 必须递增代次");

        // 条件/批量移除同样递增（即使未命中条目——宁可早失效；两者均有
        // 回执，返回即已应用）
        handle.remove_all_by_container_id("absent").await;
        let after_batch = handle.list_epoch();
        assert!(after_batch > after_remove, "批量移除递增代次");
        handle.remove_if_container_id("missing", "id-none").await;
        let after_conditional = handle.list_epoch();
        assert!(after_conditional > after_batch, "条件移除递增代次");

        // 只读操作不递增
        assert!(handle.get("missing").await.is_none());
        assert_eq!(handle.list_epoch(), after_conditional, "get 不改变代次");
    }

    #[tokio::test]
    async fn late_inspect_cannot_replace_new_physical_container() {
        let (actor, handle) = ContainerStateActor::new();
        tokio::spawn(actor.run());
        let old = minimal_info("builder");
        let mut replacement = old.clone();
        replacement.container_id = "replacement-id".into();
        let old_id = old.container_id.clone();
        handle.insert("builder".into(), old.clone()).await;
        handle.insert("builder".into(), replacement.clone()).await;
        assert!(!handle.update_if_container_id("builder", &old_id, old).await);
        assert_eq!(
            handle
                .get("builder")
                .await
                .expect("replacement")
                .container_id,
            replacement.container_id
        );
    }
}
