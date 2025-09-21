# Claude Code 架构说明

## 🏗️ 整体架构

根据 [Zed 官方文档](https://zed.dev/docs/ai/external-agents) 和实际测试，Zed 使用 Claude Code 的架构如下：

```
你的 Rust 应用
    ↓ ACP 协议
你的 acp-adapter (Rust)
    ↓ 进程调用
@zed-industries/claude-code-acp (Node.js 适配器)
    ↓ 调用本地命令
claude 命令 (你本地安装的)
    ↓ 使用你的配置
Anthropic API
```

## 🔑 关键要点

### 1. **不需要额外配置**
- ❌ **不需要配置国内大模型 URL**
- ❌ **不需要在 Rust 代码中配置 API Key**
- ❌ **不需要配置模型名称**
- ✅ **使用你本地 Claude Code 的所有配置**

### 2. **认证方式**
- 在 Claude Code 线程中运行 `/login` 命令
- 可以选择：
  - API Key 认证
  - "Log in with Claude Code"（使用 Claude Pro/Max 订阅）
- **Zed 不会使用你在 Zed Agent 设置中的 Anthropic API Key**

### 3. **配置来源**
- 使用你本地 `claude` 命令的配置
- 支持 `CLAUDE.md` 文件
- 支持本地 Claude Code 的所有功能

## 📋 你的配置

### 当前配置（正确）
```rust
// crates/acp_adapter/src/config.rs
pub fn claude_code() -> Self {
    Self::new(
        "claude".to_string(),
        "npx".to_string(),
    )
    .with_args(vec![
        "@zed-industries/claude-code-acp".to_string()
    ])
    .with_mcp_enabled(true)
    // 注意：没有设置 API Key，因为 claude-code-acp 会调用本地 claude 命令
}
```

### 为什么这样配置？

1. **`npx @zed-industries/claude-code-acp`**：
   - 使用 Zed 官方维护的 ACP 适配器
   - 自动处理与本地 `claude` 命令的通信

2. **不设置 API Key**：
   - `claude-code-acp` 会调用你本地的 `claude` 命令
   - 本地 `claude` 命令已经处理了认证
   - 避免了重复配置

3. **MCP 启用**：
   - 支持 Model Context Protocol
   - 提供更丰富的功能

## 🚀 使用步骤

### 1. 确保本地 Claude Code 已安装和认证
```bash
# 检查安装
claude --version

# 检查认证状态
claude auth status

# 如果需要认证
claude auth login
```

### 2. 在你的 Rust 代码中使用
```rust
use acp_adapter::{AcpAdapter, AcpConfig};

let config = AcpConfig::claude_code()
    .with_working_dir(PathBuf::from("."));

let adapter = AcpAdapter::new(config);
adapter.initialize().await?;

let session = adapter.create_session().await?;
// 使用会话...
```

### 3. 在 Zed 中使用（参考）
1. 打开 Agent Panel (cmd-? 或 ctrl-?)
2. 点击 + 按钮，选择 'New Claude Code Thread'
3. 在新线程中运行: `/login`
4. 选择你的认证方式
5. 开始使用！

## 🔍 验证方法

### 运行测试脚本
```bash
# 测试本地集成
./test_local_claude_integration.sh

# 测试配置
./test_config_only.sh

# 运行示例
cargo run --package claude_code_acp_example --bin claude_code_acp_example
```

## 📊 优势

1. **简化配置**：不需要在 Rust 代码中管理 API Key
2. **使用本地配置**：利用你已有的 Claude Code 设置
3. **官方支持**：使用 Zed 官方维护的适配器
4. **功能完整**：支持所有 Claude Code 功能
5. **自动更新**：通过 npm 自动获取最新版本

## 🎯 总结

你的配置是正确的！通过 `claude-code-acp` 调用本地 `claude` 命令是 Zed 官方推荐的方式，这样：

- 不需要配置国内大模型 URL
- 不需要在代码中设置 API Key
- 使用你本地 Claude Code 的所有配置和认证
- 获得完整的功能支持

这就是为什么 Zed 能够无缝集成 Claude Code 的原因！
