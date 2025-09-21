# Claude Code ACP 集成指南

本文档说明如何使用 Zed 的 `claude-code-acp` 适配器来集成 Claude Code。

## 架构概述

```
你的 Rust 代码 (ACP 客户端)
    ↓ ACP 协议
claude-code-acp (Node.js 适配器)
    ↓ 调用
Claude Code (Anthropic 官方工具)
```

## 前置要求

1. **Node.js 和 npm**：需要安装 Node.js 和 npm 来运行 `claude-code-acp`
2. **Claude API Key**：需要设置 `CLAUDE_API_KEY` 环境变量
3. **Claude Code**：需要安装 Claude Code 工具

## 安装步骤

### 1. 安装 Claude Code

```bash
# 安装 Claude Code
npm install -g @anthropic-ai/claude-code

# 验证安装
claude --version
```

### 2. 设置环境变量

```bash
# 设置 Claude API Key
export CLAUDE_API_KEY="your_api_key_here"

# 验证设置
echo $CLAUDE_API_KEY
```

### 3. 安装 claude-code-acp

```bash
# 全局安装（可选）
npm install -g @zed-industries/claude-code-acp

# 或者使用 npx（推荐）
npx @zed-industries/claude-code-acp --help
```

## 使用方法

### 基本使用

```rust
use acp_adapter::{AcpAdapter, AcpConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 创建配置
    let config = AcpConfig::claude_code()
        .with_working_dir(PathBuf::from("."))
        .with_env("CLAUDE_API_KEY".to_string(), 
                 std::env::var("CLAUDE_API_KEY").unwrap_or_default());

    // 创建适配器
    let adapter = AcpAdapter::new(config);
    
    // 初始化
    adapter.initialize().await?;
    
    // 创建会话
    let session = adapter.create_session().await?;
    
    // 使用会话...
    
    Ok(())
}
```

### 运行示例

```bash
# 运行示例
cargo run --example claude_code_acp_example

# 或者运行测试脚本
./test_claude_code_acp.sh
```

## 配置选项

### AcpConfig::claude_code() 默认配置

- **命令**: `npx`
- **参数**: `["@zed-industries/claude-code-acp"]`
- **MCP 启用**: `true`
- **认证**: 使用 `CLAUDE_API_KEY` 环境变量

### 自定义配置

```rust
let config = AcpConfig::claude_code()
    .with_working_dir(PathBuf::from("/path/to/project"))
    .with_env("CUSTOM_VAR".to_string(), "value".to_string())
    .with_timeout(120)  // 2分钟超时
    .with_mcp_enabled(true);
```

## 优势

1. **标准化**：使用 Zed 官方维护的适配器
2. **功能完整**：支持所有 ACP 协议功能
3. **自动更新**：通过 npm 自动获取最新版本
4. **兼容性好**：与 Zed 生态系统完全兼容
5. **维护成本低**：不需要自己实现复杂的 ACP 协议处理

## 故障排除

### 常见问题

1. **Node.js 未安装**
   ```bash
   # 安装 Node.js
   curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
   nvm install node
   ```

2. **CLAUDE_API_KEY 未设置**
   ```bash
   export CLAUDE_API_KEY="your_api_key_here"
   ```

3. **网络连接问题**
   ```bash
   # 检查网络连接
   ping api.anthropic.com
   ```

4. **权限问题**
   ```bash
   # 确保有执行权限
   chmod +x test_claude_code_acp.sh
   ```

### 调试模式

```bash
# 启用详细日志
RUST_LOG=debug cargo run --example claude_code_acp_example

# 或者
RUST_LOG=acp_adapter=debug cargo run --example claude_code_acp_example
```

## 与直接调用 Claude Code 的区别

| 特性 | 直接调用 | 通过 claude-code-acp |
|------|----------|---------------------|
| 实现复杂度 | 高 | 低 |
| 功能完整性 | 需要自己实现 | 完整支持 |
| 维护成本 | 高 | 低 |
| 更新频率 | 手动 | 自动 |
| 兼容性 | 需要适配 | 原生支持 |
| 错误处理 | 需要自己实现 | 内置处理 |

## 总结

使用 `claude-code-acp` 适配器是推荐的方式，因为它：

- 减少了实现复杂度
- 提供了完整的功能支持
- 降低了维护成本
- 确保了与 Zed 生态系统的兼容性

通过这种方式，你可以专注于业务逻辑的实现，而不需要担心底层的 ACP 协议细节。
