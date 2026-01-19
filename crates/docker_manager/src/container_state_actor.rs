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
    /// 条件移除：只有当 container_id 匹配时才移除
    RemoveIfContainerId {
        key: String,
        container_id: String,
        reply: oneshot::Sender<Option<DockerContainerInfo>>,
    },
}

/// 容器状态 Actor
///
/// 独占 HashMap，在独立 task 中运行
pub struct ContainerStateActor {
    containers: HashMap<String, DockerContainerInfo>,
    receiver: mpsc::Receiver<ContainerStateCommand>,
}

impl ContainerStateActor {
    /// 创建新的 Actor 和 Handle
    pub fn new() -> (Self, ContainerStateHandle) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let actor = Self {
            containers: HashMap::new(),
            receiver,
        };
        let handle = ContainerStateHandle { sender };
        (actor, handle)
    }

    /// 运行 Actor 事件循环
    ///
    /// 这个方法应该在 `tokio::spawn` 中调用
    pub async fn run(mut self) {
        debug!("🚀 [ACTOR] ContainerStateActor started");

        while let Some(cmd) = self.receiver.recv().await {
            self.handle_command(cmd);
        }

        debug!("🛑 [ACTOR] ContainerStateActor stopped (all senders dropped)");
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
            }
            ContainerStateCommand::Remove { key, reply } => {
                let result = self.containers.remove(&key);
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
                let existed = if let std::collections::hash_map::Entry::Occupied(mut e) = self.containers.entry(key) {
                    e.insert(updated_info);
                    true
                } else {
                    false
                };
                if reply.send(existed).is_err() {
                    warn!("[ACTOR] UpdateWith reply channel closed");
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

                if reply.send(result).is_err() {
                    warn!("[ACTOR] RemoveIfContainerId reply channel closed");
                }
            }
        }
    }
}

/// 容器状态句柄
///
/// 可克隆，用于与 Actor 通信
#[derive(Clone)]
pub struct ContainerStateHandle {
    sender: mpsc::Sender<ContainerStateCommand>,
}

impl ContainerStateHandle {
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
        rx.await.unwrap_or(None)
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
        rx.await.unwrap_or(None)
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
        rx.await.unwrap_or_default()
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
        rx.await.unwrap_or_default()
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
        rx.await.unwrap_or(0)
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
        rx.await.unwrap_or(false)
    }

    /// 条件更新：如果 key 存在则更新
    ///
    /// 返回 true 表示更新成功，false 表示 key 不存在
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
        rx.await.unwrap_or(false)
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
        rx.await.unwrap_or(None)
    }
}

impl std::fmt::Debug for ContainerStateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerStateHandle")
            .field("sender_closed", &self.sender.is_closed())
            .finish()
    }
}

// Tests removed intentionally.
// Integration tests should be implemented in `tests/` directory if needed.
