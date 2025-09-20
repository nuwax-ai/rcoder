//! 进程管理模块

use crate::{AcpAdapterError, AcpResult};
use tokio::process::{Command, Child, ChildStdin, ChildStdout};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// 进程句柄
#[derive(Clone)]
pub struct ProcessHandle {
    id: String,
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<BufWriter<ChildStdin>>>>,
    stdout: Arc<Mutex<Option<BufReader<ChildStdout>>>>,
    state: Arc<Mutex<ProcessState>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessState {
    NotStarted,
    Running,
    Stopped,
    Failed(String),
}

impl ProcessHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn state(&self) -> ProcessState {
        self.state.lock().await.clone()
    }

    pub async fn is_running(&self) -> bool {
        matches!(self.state().await, ProcessState::Running)
    }

    pub async fn write_line(&self, line: &str) -> AcpResult<()> {
        let mut stdin = self.stdin.lock().await;
        if stdin.is_none() {
            return Err(AcpAdapterError::process("进程输入流未初始化"));
        }

        let writer = stdin.as_mut().unwrap();
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        debug!("发送到进程: {}", line);
        Ok(())
    }

    pub async fn read_line(&self) -> AcpResult<Option<String>> {
        let mut stdout = self.stdout.lock().await;
        if stdout.is_none() {
            return Err(AcpAdapterError::process("进程输出流未初始化"));
        }

        let reader = stdout.as_mut().unwrap();
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            return Ok(None);
        }

        // 去除换行符
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }

        debug!("从进程接收: {}", line);
        Ok(Some(line))
    }

    pub async fn kill(&self) -> AcpResult<()> {
        let mut child = self.child.lock().await;
        if let Some(mut process) = child.take() {
            process.kill().await?;
            *self.state.lock().await = ProcessState::Stopped;
            info!("进程 {} 已被终止", self.id);
        }
        Ok(())
    }

    pub async fn wait(&self) -> AcpResult<std::process::ExitStatus> {
        let mut child = self.child.lock().await;
        if let Some(mut process) = child.take() {
            let status = process.wait().await?;
            *self.state.lock().await = ProcessState::Stopped;
            Ok(status)
        } else {
            Err(AcpAdapterError::process("进程未运行"))
        }
    }
}

/// 进程管理器
pub struct ProcessManager {
    processes: Arc<dashmap::DashMap<String, ProcessHandle>>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub async fn spawn_process(
        &self,
        config: &crate::config::ProcessConfig,
    ) -> AcpResult<ProcessHandle> {
        let process_id = uuid::Uuid::new_v4().to_string();

        // 构建命令
        let mut command = Command::new(&config.command);
        command.args(&config.args);

        // 设置环境变量
        for (key, value) in &config.env {
            command.env(key, value);
        }

        // 设置工作目录
        if let Some(working_dir) = &config.working_dir {
            command.current_dir(working_dir);
        }

        // 设置标准输入输出
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        info!("启动进程: {} {}", config.command, config.args.join(" "));

        // 启动进程
        let mut child = command
            .spawn()
            .map_err(|e| AcpAdapterError::process(format!("启动进程失败: {}", e)))?;

        // 获取输入输出流
        let stdin = child.stdin.take().ok_or_else(|| {
            AcpAdapterError::process("无法获取进程输入流")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AcpAdapterError::process("无法获取进程输出流")
        })?;

        // 创建进程句柄
        let handle = ProcessHandle {
            id: process_id.clone(),
            child: Arc::new(Mutex::new(Some(child))),
            stdin: Arc::new(Mutex::new(Some(BufWriter::new(stdin)))),
            stdout: Arc::new(Mutex::new(Some(BufReader::new(stdout)))),
            state: Arc::new(Mutex::new(ProcessState::Running)),
        };

        // 注册进程
        self.processes.insert(process_id.clone(), handle.clone());

        // 启动进程监控
        self.monitor_process(process_id.clone(), config.restart_on_failure, config.max_restarts);

        info!("进程启动成功: {}", process_id);
        Ok(handle)
    }

    fn monitor_process(
        &self,
        process_id: String,
        restart_on_failure: bool,
        max_restarts: Option<u32>,
    ) {
        let processes = self.processes.clone();

        tokio::spawn(async move {
            let mut restart_count = 0;
            let mut no_process_count = 0;

            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                let handle = if let Some(handle) = processes.get(&process_id) {
                    handle.clone()
                } else {
                    no_process_count += 1;
                    // 如果进程不存在超过3次循环，退出监控
                    if no_process_count > 3 {
                        break;
                    }
                    continue;
                };

                // 重置计数器
                no_process_count = 0;

                let current_state = handle.state().await;
                match current_state {
                    ProcessState::Running => continue,
                    ProcessState::Failed(_) => {
                        if restart_on_failure {
                            if let Some(max) = max_restarts {
                                if restart_count >= max {
                                    warn!("进程 {} 已达到最大重启次数", process_id);
                                    break;
                                }
                            }

                            restart_count += 1;
                            warn!("进程 {} 意外退出，准备重启 (重启次数: {})", process_id, restart_count);

                            // 移除旧进程
                            processes.remove(&process_id);

                            // TODO: 这里应该重新启动进程，但需要配置信息
                            // 暂时先 break
                            break;
                        } else {
                            warn!("进程 {} 已停止", process_id);
                            break;
                        }
                    }
                    ProcessState::Stopped | ProcessState::NotStarted => {
                        break;
                    }
                }
            }

            debug!("进程监控器 {} 已退出", process_id);
        });
    }

    pub fn get_process(&self, process_id: &str) -> Option<ProcessHandle> {
        self.processes.get(process_id).map(|h| h.clone())
    }

    pub async fn kill_process(&self, process_id: &str) -> AcpResult<()> {
        if let Some(handle) = self.processes.get(process_id) {
            handle.kill().await?;
            self.processes.remove(process_id);
            info!("进程 {} 已被终止并移除", process_id);
        } else {
            return Err(AcpAdapterError::process(format!("进程 {} 不存在", process_id)));
        }
        Ok(())
    }

    pub async fn kill_all(&self) -> AcpResult<()> {
        let process_ids: Vec<String> = self.processes.iter().map(|p| p.key().clone()).collect();

        for process_id in process_ids {
            if let Err(e) = self.kill_process(&process_id).await {
                error!("终止进程 {} 失败: {}", process_id, e);
            }
        }

        Ok(())
    }

    pub fn list_processes(&self) -> Vec<(String, ProcessState)> {
        self.processes
            .iter()
            .map(|p| {
                let process_id = p.key().clone();
                let state = p.value().state();
                let handle = p.value().clone();

                // 使用 block_on 来获取异步状态
                let current_state = futures::executor::block_on(async { state.await });

                (process_id, current_state)
            })
            .collect()
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 消息流处理器
pub struct MessageStream {
    process_handle: ProcessHandle,
    shutdown_tx: mpsc::Sender<()>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
}

impl MessageStream {
    pub fn new(process_handle: ProcessHandle) -> Self {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        Self {
            process_handle,
            shutdown_tx,
            shutdown_rx: Some(shutdown_rx),
        }
    }

    pub async fn read_messages<F>(&mut self, mut callback: F) -> AcpResult<()>
    where
        F: FnMut(String) -> AcpResult<()>,
    {
        let mut shutdown_rx = self.shutdown_rx.take().unwrap();

        loop {
            tokio::select! {
                // 检查关闭信号
                _ = shutdown_rx.recv() => {
                    info!("消息流处理器收到关闭信号");
                    break;
                }

                // 读取进程输出
                result = self.process_handle.read_line() => {
                    match result {
                        Ok(Some(line)) => {
                            if let Err(e) = callback(line) {
                                error!("处理消息失败: {}", e);
                            }
                        }
                        Ok(None) => {
                            info!("进程输出流已关闭");
                            break;
                        }
                        Err(e) => {
                            error!("读取进程输出失败: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn shutdown(&self) -> AcpResult<()> {
        let _ = self.shutdown_tx.send(()).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_manager() {
        let manager = ProcessManager::new();

        // 创建一个立即退出的进程配置
        let config = crate::config::ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            env: std::collections::HashMap::new(),
            working_dir: None,
            timeout_seconds: Some(5),
            restart_on_failure: false, // 禁用重启以避免监控器长时间运行
            max_restarts: None,
        };

        // 启动进程
        let handle = manager.spawn_process(&config).await.unwrap();
        let handle_id = handle.id().to_string();

        // 等待进程自然退出（echo会立即退出）
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 手动移除进程以停止监控器
        manager.processes.remove(&handle_id);

        // 清理
        manager.kill_all().await.unwrap();
    }

    #[tokio::test]
    async fn test_message_stream() {
        let manager = ProcessManager::new();

        // 创建一个产生单行输出的进程
        let config = crate::config::ProcessConfig {
            command: "echo".to_string(),
            args: vec!["test_message".to_string()],
            env: std::collections::HashMap::new(),
            working_dir: None,
            timeout_seconds: Some(2),
            restart_on_failure: false, // 禁用重启以避免监控器长时间运行
            max_restarts: None,
        };

        let handle = manager.spawn_process(&config).await.unwrap();
        let mut stream = MessageStream::new(handle.clone());

        let mut message_count = 0;
        let result = stream.read_messages(|_line| {
            message_count += 1;
            Ok(()) // 简单处理所有消息
        }).await;

        // 应该成功完成
        assert!(result.is_ok());
        assert!(message_count >= 1);

        // 清理
        handle.kill().await.unwrap();
    }

    #[tokio::test]
    async fn test_process_lifecycle() {
        // 测试进程生命周期而不使用监控器
        let mut command = tokio::process::Command::new("echo");
        command.arg("test");
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn().unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }
}