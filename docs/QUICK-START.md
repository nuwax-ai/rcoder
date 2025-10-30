# 快速开始

## 编译和运行

### 默认方式（推荐）

使用 Claude Agent，避免 Codex 的编译问题：

```bash
# 编译
cargo build

# 运行
cargo run

# 或者直接运行
cargo run -p rcoder
```

### 开发模式

```bash
# 监听文件变化并自动重新编译
cargo watch -x run

# 运行测试
cargo test

# 检查代码
cargo check
```

## 配置

### 环境变量

```bash
# 设置服务端口
export RCODER_PORT=8086

# Claude 配置
export ANTHROPIC_BASE_URL=https://api.anthropic.com
export ANTHROPIC_AUTH_TOKEN=your_token_here
export ANTHROPIC_MODEL=claude-3-5-sonnet-20241022
```

### 配置文件

首次运行时会自动创建 `config.yml`：

```yaml
# 默认使用的 AI 代理类型
default_agent: Claude

# 项目工作目录
projects_dir: ./project_workspace

# 主服务端口
port: 8086
```

## 常见问题

### Q: 如何切换到 Codex？

A: 当前由于 Codex 上游依赖问题，暂时无法使用。建议使用 Claude Agent。

### Q: 编译很慢怎么办？

A: 使用增量编译和缓存：
```bash
# 只检查不编译
cargo check

# 只编译库
cargo build --lib

# 使用 sccache 加速
cargo install sccache
export RUSTC_WRAPPER=sccache
```

### Q: 如何查看日志？

A: 设置 RUST_LOG 环境变量：
```bash
export RUST_LOG=debug
cargo run
```

## 下一步

- 查看 [API 文档](./api-docs.md)
- 了解 [Codex Feature](./codex-feature.md)
- 阅读 [架构设计](./architecture.md)
