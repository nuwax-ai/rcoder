//! 优雅关闭信号处理模块

use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};
use futures::StreamExt;

/// 关闭信号
#[derive(Debug, Clone, PartialEq)]
pub enum ShutdownSignal {
    /// 停止信号
    Stop,
    /// 重启信号
    Restart,
    /// 强制关闭信号
    ForceStop,
}

/// 关闭管理器
#[derive(Clone)]
pub struct ShutdownManager {
    /// 关闭信号发送器
    shutdown_tx: mpsc::Sender<ShutdownSignal>,
    /// 关闭信号接收器
    shutdown_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<ShutdownSignal>>>>,
    /// 广播发送器
    broadcast_tx: broadcast::Sender<ShutdownSignal>,
}

impl ShutdownManager {
    /// 创建新的关闭管理器
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(100);
        let (broadcast_tx, _) = broadcast::channel(100);

        Self {
            shutdown_tx,
            shutdown_rx: Arc::new(tokio::sync::Mutex::new(Some(shutdown_rx))),
            broadcast_tx,
        }
    }

    /// 发送关闭信号
    pub async fn send_shutdown(&self, signal: ShutdownSignal) -> Result<(), mpsc::error::SendError<ShutdownSignal>> {
        info!("发送关闭信号: {:?}", signal);

        // 发送到主通道
        if let Err(e) = self.shutdown_tx.send(signal.clone()).await {
            warn!("发送关闭信号到主通道失败: {}", e);
        }

        // 广播给所有监听者
        if let Err(e) = self.broadcast_tx.send(signal.clone()) {
            warn!("广播关闭信号失败: {}", e);
        }

        Ok(())
    }

    /// 订阅关闭信号
    pub fn subscribe(&self) -> broadcast::Receiver<ShutdownSignal> {
        self.broadcast_tx.subscribe()
    }

    /// 获取关闭信号接收器
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<ShutdownSignal>> {
        self.shutdown_rx.lock().await.take()
    }

    /// 等待关闭信号
    pub async fn wait_for_shutdown(&self) -> ShutdownSignal {
        let mut rx = self.subscribe();
        match rx.recv().await {
            Ok(signal) => {
                info!("收到关闭信号: {:?}", signal);
                signal
            }
            Err(e) => {
                error!("接收关闭信号失败: {}", e);
                ShutdownSignal::ForceStop
            }
        }
    }
}

impl Default for ShutdownManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 信号处理器
pub struct SignalHandler {
    shutdown_manager: ShutdownManager,
    _signal_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SignalHandler {
    /// 创建新的信号处理器
    pub fn new(shutdown_manager: ShutdownManager) -> Result<Self, std::io::Error> {
        let mut signal_handles = Vec::new();

        #[cfg(unix)]
        {
            use signal_hook::consts::SIGTERM;
            use signal_hook_tokio::Signals;
            use futures::StreamExt;

            // 处理 SIGTERM 信号
            let mut signals = Signals::new(&[SIGTERM, signal_hook::consts::SIGINT])?;
            let shutdown_manager_clone = shutdown_manager.clone();

            let handle = tokio::spawn(async move {
                while let Some(signal) = signals.next().await {
                    debug!("收到系统信号: {}", signal);
                    let shutdown_signal = match signal {
                        SIGTERM => ShutdownSignal::Stop,
                        signal_hook::consts::SIGINT => ShutdownSignal::ForceStop,
                        _ => continue,
                    };

                    if let Err(e) = shutdown_manager_clone.send_shutdown(shutdown_signal).await {
                        error!("发送关闭信号失败: {}", e);
                    }
                    break;
                }
            });

            signal_handles.push(handle);
        }

        Ok(Self {
            shutdown_manager,
            _signal_handles: signal_handles,
        })
    }

    /// 获取关闭管理器
    pub fn shutdown_manager(&self) -> &ShutdownManager {
        &self.shutdown_manager
    }
}

/// 优雅关闭工具结构
pub struct GracefulShutdown {
    shutdown_manager: ShutdownManager,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    shutdown_timeout: std::time::Duration,
}

impl GracefulShutdown {
    /// 创建新的优雅关闭实例
    pub fn new(shutdown_manager: ShutdownManager, shutdown_timeout_secs: u64) -> Self {
        Self {
            shutdown_manager,
            tasks: Vec::new(),
            shutdown_timeout: std::time::Duration::from_secs(shutdown_timeout_secs),
        }
    }

    /// 添加需要关闭的任务
    pub fn add_task(&mut self, task: tokio::task::JoinHandle<()>) {
        self.tasks.push(task);
    }

    /// 等待关闭信号并执行优雅关闭
    pub async fn wait_and_shutdown(self) {
        info!("等待关闭信号...");

        let signal = self.shutdown_manager.wait_for_shutdown().await;
        info!("开始优雅关闭: {:?}", signal);

        // 根据信号类型选择关闭策略
        match signal {
            ShutdownSignal::Stop => {
                self.graceful_shutdown().await;
            }
            ShutdownSignal::Restart => {
                self.graceful_shutdown().await;
                info!("服务重启准备完成");
            }
            ShutdownSignal::ForceStop => {
                self.force_shutdown().await;
            }
        }
    }

    /// 优雅关闭
    async fn graceful_shutdown(self) {
        info!("开始优雅关闭，超时时间: {:?}", self.shutdown_timeout);

        // 广播关闭信号
        let _ = self.shutdown_manager.send_shutdown(ShutdownSignal::Stop).await;

        // 等待所有任务完成或超时
        let start_time = std::time::Instant::now();
        let mut remaining_tasks = self.tasks;

        while !remaining_tasks.is_empty() && start_time.elapsed() < self.shutdown_timeout {
            let (finished, rest): (Vec<_>, Vec<_>) = remaining_tasks
                .into_iter()
                .partition(|task| task.is_finished());

            for task in finished {
                match task.await {
                    Ok(()) => debug!("任务正常结束"),
                    Err(e) => warn!("任务结束时出错: {}", e),
                }
            }

            remaining_tasks = rest;

            if !remaining_tasks.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        // 强制结束剩余任务
        for task in remaining_tasks {
            task.abort();
            match task.await {
                Ok(()) => debug!("任务在强制关闭前结束"),
                Err(e) => {
                    if e.is_cancelled() {
                        debug!("任务被强制取消");
                    } else {
                        warn!("强制关闭任务出错: {}", e);
                    }
                }
            }
        }

        info!("优雅关闭完成");
    }

    /// 强制关闭
    async fn force_shutdown(self) {
        info!("开始强制关闭");

        // 立即取消所有任务
        for task in self.tasks {
            task.abort();
        }

        info!("强制关闭完成");
    }
}