# Codex Feature 使用说明

## 概述

项目现在支持通过 `codex` feature 来控制是否启用 Codex 相关功能。这样可以：

1. **避免编译问题**：当 Codex 依赖出现问题时，可以禁用该功能继续开发
2. **减少编译时间**：不需要 Codex 时可以跳过相关依赖的编译
3. **灵活配置**：根据实际需求选择性启用功能

## 使用方法

### 不启用 Codex（默认，推荐）

```bash
# 编译项目（不包含 Codex 功能）
cargo build

# 运行项目
cargo run

# 注意：codex-acp-agent 不会被编译
```

### 启用 Codex（当前有编译问题）

⚠️ **警告**：由于 Codex 上游依赖的线程安全问题，当前无法成功编译。

如果上游问题已修复，可以这样启用：

```bash
# 编译 codex-acp-agent
cargo build -p codex-acp-agent

# 编译项目（包含 Codex 功能）
cargo build --features codex

# 运行项目
cargo run --features codex

# 运行特定的 crate
cargo run -p rcoder --features codex
cargo run -p agent_runner --features codex
```

### 在 Cargo.toml 中配置

如果你想默认启用 Codex 功能，可以在根目录的 `Cargo.toml` 中修改：

```toml
[workspace.features]
default = ["codex"]  # 默认启用 codex
codex = []
```

## 功能说明

### 启用 Codex 时

- 支持 `AgentType::Codex` 类型
- 可以使用 Codex ACP Agent
- 包含 codex-core、codex-common、codex-arg0 等依赖

### 禁用 Codex 时

- 只支持 `AgentType::Claude` 类型
- 不会编译 Codex 相关代码
- 减少依赖和编译时间

## 相关文件

- `Cargo.toml` - workspace 级别的 feature 配置
- `crates/shared_types/Cargo.toml` - shared_types 的 feature 配置
- `crates/agent_runner/Cargo.toml` - agent_runner 的 feature 配置
- `crates/rcoder/Cargo.toml` - rcoder 的 feature 配置

## 注意事项

1. 如果遇到 Codex 相关的编译错误，可以先禁用 codex feature 继续开发
2. 在生产环境中，根据实际需求决定是否启用 codex feature
3. 代码中使用 `#[cfg(feature = "codex")]` 来条件编译 Codex 相关功能

## 故障排除

### 问题：编译时出现 `Rc<Box<[u8]>>` 线程安全错误

这是 Codex 上游依赖 (`codex-protocol` -> `icu_decimal`) 的已知问题。

**原因**：`icu_decimal` 默认使用 `Rc` 而不是 `Arc`，导致在多线程环境下无法编译。

**解决方案**：

1. **推荐方案**：禁用 codex feature，使用 Claude Agent
   ```bash
   cargo build
   cargo run
   ```

2. **临时方案**：等待 Codex 上游修复此问题
   - 跟踪 issue: https://github.com/zed-industries/codex/issues
   - 或者联系 Codex 维护者

3. **高级方案**：Fork codex 并修复
   - Fork https://github.com/zed-industries/codex
   - 在 `codex-rs/protocol/Cargo.toml` 中为 `icu_decimal` 启用 `sync` feature
   - 更新项目依赖指向你的 fork

### 问题：找不到 AgentType::Codex

确保启用了 codex feature：
```bash
cargo build --features codex
```

### 问题：默认想使用 Codex 但编译失败

当前由于上游问题，建议：
1. 使用 Claude Agent（默认且稳定）
2. 等待 Codex 上游修复线程安全问题
3. 或者贡献代码修复上游问题
