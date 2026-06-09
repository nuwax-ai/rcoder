# ACP Agent 安装目录与 Dockerfile 配合说明

## 背景

`agent_runner` 的 ACP agent 安装器支持两种包类型：

| 类型 | 示例 | binary_path | command |
|------|------|-------------|---------|
| 二进制型 | `codex-acp` | 入口文件绝对路径 | `codex-acp` |
| 目录型 | `deepagents-dev-templates` | agent 安装目录 | `node` |

## 安装目录结构

```
/home/user/acp-agent/                  ← install_dir (PathManager)
├── registry.json                      ← 已安装 agent 注册表
├── codex-acp/                         ← 二进制型 agent
│   ├── codex-acp                      ← 入口可执行文件
│   └── ...
└── deepagents-app-agent/              ← 目录型 agent
    ├── dist/index.js                  ← 入口脚本 (bin.start)
    ├── node_modules/                  ← 依赖
    ├── agent-package.json             ← agent 元数据
    └── package.json
```

## Dockerfile 需要的改动

### 当前配置（已有）

```dockerfile
# Dockerfile:22
ENV PATH="/home/user/acp-agent:$PATH"
```

这行把 `/home/user/acp-agent` 加入 PATH，但**只能找到直接在该目录下的可执行文件**，
对于子目录中的 agent（如 `codex-acp/codex-acp`）无效。

### 需要添加：动态 PATH 注入脚本

在 `start-up.sh` 中添加以下逻辑，启动时扫描并注入已安装 agent 的目录到 PATH：

```bash
# === ACP Agent PATH 注入 ===
# 扫描 /home/user/acp-agent/ 下的子目录，将包含可执行文件的目录加入 PATH
ACPP_AGENT_DIR="/home/user/acp-agent"
if [ -d "$ACPP_AGENT_DIR" ]; then
    for agent_subdir in "$ACPP_AGENT_DIR"/*/; do
        if [ -d "$agent_subdir" ]; then
            export PATH="${agent_subdir}${PATH:+:$PATH}"
        fi
    done
fi
```

这样每个 agent 的子目录都会被加入 PATH，`which codex-acp` 就能找到
`/home/user/acp-agent/codex-acp/codex-acp`。

### 目录型 agent 不需要额外配置

目录型 agent（Node.js / Bun / Python）的 command 是解释器（如 `node`），
已经在系统 PATH 中。入口脚本路径（如 `dist/index.js`）通过 args 传递，
工作目录由 launcher 设置为 agent 安装目录。

## /computer/chat 调用示例

### 二进型 agent

```json
{
  "user_id": "user1",
  "prompt": "hello",
  "agent_config": {
    "agent_server": {
      "command": "codex-acp",
      "args": [],
      "env": {}
    }
  }
}
```

### 目录型 agent（Node.js）

```json
{
  "user_id": "user1",
  "prompt": "hello",
  "agent_config": {
    "agent_server": {
      "command": "node",
      "args": ["dist/index.js"],
      "env": {
        "ANTHROPIC_API_KEY": "sk-xxx"
      }
    }
  }
}
```

## /agent-mgmt/agents/install-from-url 调用示例

```json
{
  "agent_id": "deepagents-app-agent",
  "command": "node",
  "version": "0.2.9",
  "platforms": {
    "linux-x86_64": {
      "url": "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/test-upload/deepagents-dev-templates-0.2.9-nuwax.tar.gz"
    }
  }
}
```

安装后，registry.json 中的记录：

```json
{
  "agent_id": "deepagents-app-agent",
  "install_type": "Url",
  "command": "node",
  "args": [],
  "binary_path": "/home/user/acp-agent/deepagents-app-agent",
  "version": "0.2.9",
  "file_type": "tar.gz"
}
```

## 生产环境 Dockerfile 参考

```dockerfile
# 创建 ACP agent 安装目录
RUN mkdir -p /home/user/acp-agent

# PATH 注入（基础层）
ENV PATH="/home/user/acp-agent:$PATH"

# start-up.sh 中添加动态子目录注入（见上方脚本）
# 这样运行时安装的 agent 也能被 which 找到
```
