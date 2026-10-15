# 权限审批

Agent 执行工具调用（写文件、执行命令等）前，可能需要用户确认。本文说明 RCoder 的权限决策链、审批事件的流转与配置方式。本文是概念说明，字段级契约以运行时 OpenAPI 文档（`/api/docs`）与 `shared_types` 类型定义为准。

## 决策链

agent 发起权限请求（ACP `RequestPermissionRequest`）后，服务端按以下顺序决策，命中即停：

```
1. 危险命令识别        —— 仅观测：命中只记 warn 日志，不拦截、不改变决策
2. RuleStore           —— 会话内已保存的规则（用户此前勾选"记住此决策"产生）
3. tool_approval_rules —— 配置的静态规则（glob 匹配，见下节）
4. recent_resolutions  —— 同一 tool_call_id 的多个权限请求自动跟随最近一次决策
5. agent_mode          —— 兜底模式：ask（逐次询问）/ yolo（全部放行），默认 yolo
```

设计要点：

- **recent_resolutions 跟随**：一次工具调用可能触发多条权限请求；用户裁决第一条后，同批后续请求自动跟随，避免连续弹窗。
- **yolo 放行用 `AllowOnce` 而非 `AllowAlways`**：不污染 agent 自身的权限记忆，模式切换（yolo → ask）完全可逆。
- **容器环境立场**：平台不替用户自动 deny 危险命令——危险命令识别只做观测日志，最终决策权在决策链与用户。

## ask 命中时的审批流转

决策链走到 ask 时，agent 挂起，前端经 SSE 收到 `acpRequestPermission` 事件（`messageType`，子类型 `request_permission`，见[会话与 SSE](agent-sessions.md)），用户选择后客户端回传裁决：

| 端点 | 域 |
|---|---|
| `POST /agent/notify-resolved` | web agent 会话 |
| `POST /computer/notify-resolved` | Computer Agent 会话 |
| `POST /devcomputer/notify-resolved` | 开发态会话 |

回执要点：

- 请求体携带会话标识、工具调用标识（`toolCallId`）与用户选择的 `optionId`——`optionId` 原样透传给 agent，客户端不解释其语义。
- 选项语义（allow / reject / cancelled 等）由 agent 的 ACP 协议定义；**Cancelled**（用户放弃本轮）与 **RejectOnce**（明确拒绝本次）是不同的裁决。
- `saveRule=true` 时服务端将该决策存入 RuleStore，本会话内同类请求不再询问。

裁决经 rcoder 转发（gRPC `ResolvePermission`）到 agent_runner 的权限管理器，agent 继续或中止执行。

## tool_approval_rules 配置

静态规则配置在 agent 配置的 `tool_approval_rules` 数组（`agent_config.agent_server.tool_approval_rules`）：

```yaml
tool_approval_rules:
  - patterns: ["rm -rf *"]        # glob，大小写不敏感
    action: ask                   # ask / allow / deny
  - patterns: ["git *", "cargo *"]
    action: allow
    tool_kind: execute            # 可选：限定工具类别
```

- **多规则顺序优先**，首条命中即停；不配置则直接落到 agent_mode 兜底。
- **双路径匹配**：通用规则对命令族字段（command / cmd / script / 原始输入）与工具名字段（tool / tool_name / toolName）以及标题做多字段容错匹配，任一命中即触发；显式指定 `tool_kind` 时退回单字段精确匹配。
- **命令类工具**的类别集合：`execute` / `bash` / `terminal` / `shell` / `command`。

## 相关文档

- [会话与 SSE](agent-sessions.md)：`acpRequestPermission` 事件结构
- [架构总览](../architecture/overview.md)：权限决策链所在 crate（agent_runner / shared_types）
