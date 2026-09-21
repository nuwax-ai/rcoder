// bin 只做入口：`--version` 探测 + dial9 runtime 变体；完整服务组合（bootstrap →
// 引擎装配 → 路由挂载 → serve → 优雅关停）在 `rcoder::run()`。
fn version_probe() -> bool {
    std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsString::from("--version")])
}

// dial9 变体：手动构建 runtime 才能把 hooks 挂进 Builder（`#[tokio::main]`
// 做不到）。`#[hotpath::main]` 只在函数体前插 guard、不构建 runtime，可共存。
// 退出顺序：先 drop(runtime) 停事件流，再 graceful_shutdown 刷盘终段
// （5s 预算 < docker stop 10s 宽限）。
#[cfg(feature = "dial9")]
#[hotpath::main]
fn main() -> anyhow::Result<()> {
    if version_probe() {
        println!("rcoder {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (recorder, runtime) = rcoder::dial9_obs::runtime_from_env()
        .map_err(|e| anyhow::anyhow!("dial9 runtime init failed: {e}"))?;
    let exit = runtime.block_on(rcoder::run());
    drop(runtime);
    recorder.graceful_shutdown(std::time::Duration::from_secs(5));
    exit
}

#[cfg(not(feature = "dial9"))]
#[tokio::main]
#[hotpath::main]
async fn main() -> anyhow::Result<()> {
    if version_probe() {
        println!("rcoder {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    rcoder::run().await
}
