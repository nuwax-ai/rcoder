use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock, Mutex},
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Handle模式的基础trait，所有资源Handle都应该实现
pub trait ResourceHandle {
    type Id: Clone + Eq + std::hash::Hash + Send + Sync;
    type Output: Clone;

    fn id(&self) -> &Self::Id;
    fn is_finished(&self) -> bool;
    fn get_result(&self) -> Option<Result<Self::Output>>;
}

/// Terminal资源Handle - 简化版本
#[derive(Debug, Clone)]
pub struct TerminalHandle {
    pub id: TerminalId,
    pub command: String,
    pub working_dir: Option<PathBuf>,
    pub started_at: Instant,
    result: Arc<Mutex<Option<Result<TerminalOutput>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(pub String);

impl TerminalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

#[derive(Debug, Clone)]
pub struct TerminalOutput {
    pub ended_at: Instant,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub content: String,
    pub original_content_len: usize,
    pub content_line_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub enum TerminalStatus {
    Running,
    Completed(TerminalOutput),
    Failed(String),
    Killed,
}

impl ResourceHandle for TerminalHandle {
    type Id = TerminalId;
    type Output = TerminalOutput;

    fn id(&self) -> &Self::Id {
        &self.id
    }

    fn is_finished(&self) -> bool {
        if let Ok(result) = self.result.lock() {
            result.is_some()
        } else {
            false
        }
    }

    fn get_result(&self) -> Option<Result<Self::Output>> {
        if let Ok(result) = self.result.lock() {
            match result.as_ref() {
                Some(Ok(output)) => Some(Ok(output.clone())),
                Some(Err(e)) => Some(Err(anyhow::anyhow!("{}", e))),
                None => None,
            }
        } else {
            None
        }
    }
}

impl TerminalHandle {
    pub fn new(
        command: String,
        working_dir: Option<PathBuf>,
        output_byte_limit: Option<usize>,
    ) -> Self {
        let id = TerminalId::new();
        let started_at = Instant::now();
        let result = Arc::new(Mutex::new(None));
        let result_clone = result.clone();
        let command_clone = command.clone();
        let working_dir_clone = working_dir.clone();

        // 启动后台任务处理Terminal
        tokio::spawn(async move {
            let output_result = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command_clone)
                .current_dir(working_dir_clone.as_deref().unwrap_or(&std::env::current_dir().unwrap()))
                .output()
                .await;

            match output_result {
                Ok(output) => {
                    let mut stdout_content = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr_content = String::from_utf8_lossy(&output.stderr).to_string();

                    if !stderr_content.is_empty() {
                        stdout_content.push_str("\n--- STDERR ---\n");
                        stdout_content.push_str(&stderr_content);
                    }

                    let original_content_len = stdout_content.len();
                    let content_line_count = stdout_content.lines().count();
                    let mut truncated = false;

                    // 应用输出限制
                    if let Some(limit) = output_byte_limit && stdout_content.len() > limit {
                        let mut end_ix = limit.min(stdout_content.len());
                        while !stdout_content.is_char_boundary(end_ix) {
                            end_ix -= 1;
                        }
                        // 不在行中间截断
                        end_ix = stdout_content[..end_ix].rfind('\n').unwrap_or(end_ix);
                        stdout_content.truncate(end_ix);
                        truncated = true;
                    }

                    let terminal_output = TerminalOutput {
                        ended_at: Instant::now(),
                        exit_code: output.status.code(),
                        signal: None,
                        content: stdout_content,
                        original_content_len,
                        content_line_count,
                        truncated,
                    };

                    if let Ok(mut result) = result_clone.lock() {
                        *result = Some(Ok(terminal_output));
                    }
                }
                Err(e) => {
                    let error_msg = format!("Command execution failed: {}", e);
                    if let Ok(mut result) = result_clone.lock() {
                        *result = Some(Err(anyhow::anyhow!(error_msg)));
                    }
                }
            }
        });

        Self {
            id,
            command,
            working_dir,
            started_at,
            result,
        }
    }

    pub fn current_output(&self) -> Result<Option<TerminalOutput>> {
        if let Ok(result) = self.result.lock() {
            match result.as_ref() {
                Some(Ok(output)) => Ok(Some(output.clone())),
                Some(Err(e)) => Err(anyhow::anyhow!("Terminal failed: {}", e)),
                None => Ok(None), // Still running
            }
        } else {
            Err(anyhow::anyhow!("Failed to acquire result lock"))
        }
    }
}

/// 文件操作Handle
#[derive(Debug, Clone)]
pub struct FileOperationHandle {
    pub id: FileOperationId,
    pub operation_type: FileOperationType,
    pub path: PathBuf,
    pub started_at: Instant,
    result: Arc<Mutex<Option<Result<FileOperationResult>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileOperationId(pub String);

impl FileOperationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

#[derive(Debug, Clone)]
pub enum FileOperationType {
    Read,
    Write { content: String },
    Delete,
    Copy { destination: PathBuf },
    Move { destination: PathBuf },
}

#[derive(Debug, Clone)]
pub struct FileOperationResult {
    pub operation_type: FileOperationType,
    pub path: PathBuf,
    pub success: bool,
    pub content: Option<String>, // For read operations
    pub error: Option<String>,
    pub ended_at: Instant,
}

impl ResourceHandle for FileOperationHandle {
    type Id = FileOperationId;
    type Output = FileOperationResult;

    fn id(&self) -> &Self::Id {
        &self.id
    }

    fn is_finished(&self) -> bool {
        if let Ok(result) = self.result.lock() {
            result.is_some()
        } else {
            false
        }
    }

    fn get_result(&self) -> Option<Result<Self::Output>> {
        if let Ok(result) = self.result.lock() {
            match result.as_ref() {
                Some(Ok(output)) => Some(Ok(output.clone())),
                Some(Err(e)) => Some(Err(anyhow::anyhow!("{}", e))),
                None => None,
            }
        } else {
            None
        }
    }
}

impl FileOperationHandle {
    pub fn new(operation_type: FileOperationType, path: PathBuf) -> Self {
        let id = FileOperationId::new();
        let started_at = Instant::now();
        let result = Arc::new(Mutex::new(None));
        let result_clone = result.clone();
        
        // 启动后台任务执行文件操作
        let operation_type_clone = operation_type.clone();
        let path_clone = path.clone();
        tokio::spawn(async move {
            let operation_result = match &operation_type_clone {
                FileOperationType::Read => {
                    match tokio::fs::read_to_string(&path_clone).await {
                        Ok(content) => FileOperationResult {
                            operation_type: operation_type_clone,
                            path: path_clone,
                            success: true,
                            content: Some(content),
                            error: None,
                            ended_at: Instant::now(),
                        },
                        Err(e) => FileOperationResult {
                            operation_type: operation_type_clone,
                            path: path_clone,
                            success: false,
                            content: None,
                            error: Some(e.to_string()),
                            ended_at: Instant::now(),
                        },
                    }
                },
                FileOperationType::Write { content } => {
                    match tokio::fs::write(&path_clone, content).await {
                        Ok(_) => FileOperationResult {
                            operation_type: operation_type_clone,
                            path: path_clone,
                            success: true,
                            content: None,
                            error: None,
                            ended_at: Instant::now(),
                        },
                        Err(e) => FileOperationResult {
                            operation_type: operation_type_clone,
                            path: path_clone,
                            success: false,
                            content: None,
                            error: Some(e.to_string()),
                            ended_at: Instant::now(),
                        },
                    }
                },
                _ => FileOperationResult {
                    operation_type: operation_type_clone,
                    path: path_clone,
                    success: false,
                    content: None,
                    error: Some("Operation not implemented".to_string()),
                    ended_at: Instant::now(),
                },
            };
            
            if let Ok(mut result) = result_clone.lock() {
                *result = Some(Ok(operation_result));
            }
        });

        Self {
            id,
            operation_type,
            path,
            started_at,
            result,
        }
    }
}

/// 资源管理器 - 统一管理所有Handle
#[derive(Debug)]
pub struct ResourceManager {
    terminals: Arc<RwLock<HashMap<TerminalId, TerminalHandle>>>,
    file_operations: Arc<RwLock<HashMap<FileOperationId, FileOperationHandle>>>,
}

impl Default for ResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceManager {
    pub fn new() -> Self {
        Self {
            terminals: Arc::new(RwLock::new(HashMap::new())),
            file_operations: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn add_terminal(&self, handle: TerminalHandle) {
        if let Ok(mut terminals) = self.terminals.write() {
            terminals.insert(handle.id.clone(), handle);
        }
    }

    pub fn get_terminal(&self, id: &TerminalId) -> Option<TerminalHandle> {
        self.terminals.read().ok()?.get(id).cloned()
    }

    pub fn remove_terminal(&self, id: &TerminalId) -> Option<TerminalHandle> {
        self.terminals.write().ok()?.remove(id)
    }

    pub fn add_file_operation(&self, handle: FileOperationHandle) {
        if let Ok(mut file_operations) = self.file_operations.write() {
            file_operations.insert(handle.id.clone(), handle);
        }
    }

    pub fn get_file_operation(&self, id: &FileOperationId) -> Option<FileOperationHandle> {
        self.file_operations.read().ok()?.get(id).cloned()
    }

    pub fn remove_file_operation(&self, id: &FileOperationId) -> Option<FileOperationHandle> {
        self.file_operations.write().ok()?.remove(id)
    }

    pub fn cleanup_finished(&self) {
        // 清理已完成的资源
        if let Ok(mut terminals) = self.terminals.write() {
            terminals.retain(|_, handle| !handle.is_finished());
        }
        
        if let Ok(mut file_operations) = self.file_operations.write() {
            file_operations.retain(|_, handle| !handle.is_finished());
        }
    }

    pub fn list_active_terminals(&self) -> Vec<TerminalId> {
        self.terminals
            .read()
            .map(|terminals| {
                terminals
                    .iter()
                    .filter(|(_, handle)| !handle.is_finished())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn list_active_file_operations(&self) -> Vec<FileOperationId> {
        self.file_operations
            .read()
            .map(|file_operations| {
                file_operations
                    .iter()
                    .filter(|(_, handle)| !handle.is_finished())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_terminal_handle() {
        let handle = TerminalHandle::new(
            "echo Hello, World!".to_string(),
            None,
            Some(1024),
        );

        assert!(!handle.id().0.is_empty());
        assert_eq!(handle.command, "echo Hello, World!");

        // 等待命令完成
        for _ in 0..20 {
            if handle.is_finished() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        
        let result = handle.get_result();
        assert!(result.is_some());
        
        if let Some(Ok(output)) = result {
            assert!(output.content.contains("Hello, World!"));
            assert_eq!(output.exit_code, Some(0));
        }
    }

    #[tokio::test]
    async fn test_file_operation_handle() {
        let temp_path = std::env::temp_dir().join("test_file_handle.txt");
        let test_content = "Test content for handle";

        // 测试写操作
        let write_handle = FileOperationHandle::new(
            FileOperationType::Write {
                content: test_content.to_string(),
            },
            temp_path.clone(),
        );

        // 等待写操作完成
        for _ in 0..10 {
            if write_handle.is_finished() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let write_result = write_handle.get_result();
        assert!(write_result.is_some());
        assert!(write_result.unwrap().unwrap().success);

        // 测试读操作
        let read_handle = FileOperationHandle::new(
            FileOperationType::Read,
            temp_path.clone(),
        );

        // 等待读操作完成
        for _ in 0..10 {
            if read_handle.is_finished() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let read_result = read_handle.get_result();
        assert!(read_result.is_some());
        
        if let Some(Ok(result)) = read_result {
            assert!(result.success);
            assert_eq!(result.content, Some(test_content.to_string()));
        }

        // 清理
        let _ = std::fs::remove_file(temp_path);
    }

    #[tokio::test]
    async fn test_resource_manager() {
        let manager = ResourceManager::new();

        let handle = TerminalHandle::new(
            "echo test".to_string(),
            None,
            None,
        );

        let terminal_id = handle.id().clone();
        manager.add_terminal(handle);

        // 验证terminal被添加
        assert!(manager.get_terminal(&terminal_id).is_some());

        // 等待完成后清理
        sleep(Duration::from_millis(200)).await;
        manager.cleanup_finished();
    }
}