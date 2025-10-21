//! Agent Server 主入口
//!
//! 这是 Docker 容器内运行的主程序，负责启动和管理 Agent 服务

use agent_server::Cli;
use anyhow::Result;
use clap::Parser;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    // 解析命令行参数
    let cli = Cli::parse();

    // 运行主程序
    if let Err(e) = agent_server::run(cli).await {
        error!("Agent Server 运行失败: {}", e);
        std::process::exit(1);
    }

    Ok(())
}