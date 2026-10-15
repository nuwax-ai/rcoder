//! dial9 事件级 Tokio tracing 装配（`dial9` feature 专用）。
//!
//! 布尔解析由 dial9 自带 `recorder_from_env` 完成（`1/true/on` 为开，
//! `0/false/off` 为关，缺省关）：关闭时返回未挂 hook 的纯 Tokio runtime，
//! 零开销；开启时按 `DIAL9_TRACE_DIR`（默认 /tmp/dial9-traces）落盘分段
//! trace（默认 60s 轮转、1GB 预算），离线用 `dial9 serve --local-dir` 查看。

use std::io;

use dial9::AttachedRuntime;

/// 按进程环境构建 dial9 附加的 Tokio runtime。
///
/// runtime 参数沿用默认（enable_all + 多线程，与 `#[tokio::main]` 一致）。
/// 退出顺序见 main：先 drop(runtime) 停事件流，再 recorder.graceful_shutdown。
pub fn runtime_from_env() -> io::Result<AttachedRuntime> {
    // tracing 尚未 init，用 eprintln 保证可见（与 telemetry 初始化前的日志一致）
    eprintln!(
        "[dial9] DIAL9_ENABLED={} DIAL9_TRACE_DIR={:?}",
        std::env::var("DIAL9_ENABLED").unwrap_or_default(),
        std::env::var("DIAL9_TRACE_DIR").unwrap_or_else(|_| "/tmp/dial9-traces".to_string())
    );
    dial9::recorder_from_env_with(|_builder| {})
}
