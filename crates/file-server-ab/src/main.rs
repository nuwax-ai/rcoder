//! file-server A/B 对照工具：同一请求打到 Rust 与 TS 两侧，逐字段比对响应
//! （状态/头/体/工作区快照），差异按批准规则（diff-rules.json）豁免。
//!
//! 模块布局（原 8293 行单文件拆分，纯机械移动）：
//! - [`types`] 共享数据结构与常量；[`manifest`] route 覆盖清单与用例依赖
//! - [`orchestrate`] run_suite 主流程；[`http`] 请求执行层
//! - [`compare`] 响应对比引擎；[`snapshot`] 工作区终态快照对比
//! - [`assertions`] 响应体断言；[`scenarios`] 各套件场景定义
//! - [`report`] 报告产出与规则应用；[`util`] 杂项工具

#[cfg(test)]
mod tests;

mod assertions;
mod compare;
mod http;
mod manifest;
mod orchestrate;
mod report;
mod scenarios;
mod snapshot;
mod types;
mod util;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::http::client;
use crate::orchestrate::run_suite;
use crate::types::{Cli, Command, HEALTH_TIMEOUT};
use crate::util::endpoint;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { rust_url, ts_url } => doctor(&rust_url, &ts_url).await,
        Command::Run(options) => run_suite(*options).await,
    };
    if let Err(error) = result {
        eprintln!("file-server-ab: {error:#}");
        std::process::exit(1);
    }
}

async fn doctor(rust_url: &str, ts_url: &str) -> Result<()> {
    let client = client()?;
    for (side, base) in [("rust", rust_url), ("ts", ts_url)] {
        let response = client
            .get(endpoint(base, "/health"))
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("connect to {side} file-server at {base}"))?;
        if !response.status().is_success() {
            bail!("{side} health returned HTTP {}", response.status());
        }
        println!("{side}: HTTP {}", response.status());
    }
    Ok(())
}
