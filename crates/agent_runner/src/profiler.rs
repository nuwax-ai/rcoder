//! Pyroscope Profiler 集成模块
//!
//! 提供连续性能分析功能，包括：
//! - CPU Profiling
//! - Memory Profiling (Heap/Allocs)
//! - Stack trace sampling

use anyhow::{Context, Result};
use pyroscope::PyroscopeAgent;
use pyroscope_pprofrs::{PprofConfig, pprof_backend};
use tracing::{debug, info};

/// Profiler 配置
#[derive(Debug, Clone)]
pub struct ProfilerConfig {
    /// Pyroscope Server 地址
    pub server_url: String,
    /// 应用名称（支持标签格式）
    pub application_name: String,
    /// 采样频率（Hz）
    pub sample_rate: u32,
    /// 是否启用内存 profiling
    pub enable_memory: bool,
    /// 是否启用 CPU profiling
    pub enable_cpu: bool,
}

impl Default for ProfilerConfig {
    fn default() -> Self {
        Self {
            server_url: "http://pyroscope:4040".to_string(),
            application_name: "agent_runner{env=dev}".to_string(),
            sample_rate: 100,
            enable_memory: true,
            enable_cpu: true,
        }
    }
}

impl ProfilerConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(url) = std::env::var("PYROSCOPE_URL") {
            config.server_url = url;
        }

        if let Ok(name) = std::env::var("PYROSCOPE_APP_NAME") {
            config.application_name = name;
        }

        if let Ok(project_id) = std::env::var("PROJECT_ID") {
            config.application_name = format!("agent_runner{{project_id={}}}", project_id);
        }

        config
    }
}

/// Profiler Guard
///
/// 当 dropped 时自动停止 profiler
pub struct ProfilerGuard {
    _agent: Option<pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>>,
}

/// 初始化并启动 Pyroscope Profiler
///
/// # Errors
///
/// 如果 profiler 初始化失败，返回错误
pub fn init_pyroscope_profiler(config: ProfilerConfig) -> Result<ProfilerGuard> {
    if !config.enable_cpu && !config.enable_memory {
        debug!("Pyroscope profiler disabled, skipping initialization");
        return Ok(ProfilerGuard { _agent: None });
    }

    info!("Initializing Pyroscope profiler: {}", config.application_name);
    info!("  Server URL: {}", config.server_url);
    info!("  Sample rate: {} Hz", config.sample_rate);
    info!("  CPU profiling: {}", config.enable_cpu);
    info!("  Memory profiling: {}", config.enable_memory);

    // 使用正确的 API: builder() -> backend() -> build()
    let agent = PyroscopeAgent::builder(
        config.server_url,
        config.application_name,
    )
    .backend(pprof_backend(PprofConfig::new().sample_rate(config.sample_rate)))
    .build()
    .context("Failed to build Pyroscope agent")?;

    // 启动 profiling，返回 PyroscopeAgent<PyroscopeAgentRunning>
    let agent_running = agent
        .start()
        .context("Failed to start Pyroscope profiler")?;

    info!("Pyroscope profiler started successfully");

    Ok(ProfilerGuard {
        _agent: Some(agent_running),
    })
}

/// 便捷函数：使用默认配置初始化 profiler
pub fn init_pyroscope_profiler_default() -> Result<ProfilerGuard> {
    init_pyroscope_profiler(ProfilerConfig::from_env())
}
