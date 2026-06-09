# 工具审批策略配置设计

## 概述

在 `/chat`、`/computer/chat`、`/devcomputer/chat` 三个接口的 `agent_config.agent_server` 下，新增 `tool_approval_rules` 数组参数，用于精细控制工具审批行为。

- **YOLO 模式**下可配置特定工具**需要审批**（如 `rm -rf`、`DROP TABLE`）
- **ASK 模式**下可配置特定工具**自动放行**（如 `ls`、`cat`、只读查询）
- 不传或为空时，`agent_mode` 默认行为不变

### 设计目标

1. **模式无关**: 规则独立于 `agent_mode`，按配置的 `action` 生效
2. **双向覆盖**: 既可强制审批，也可强制放行
3. **工具类型感知**: 根据 `tool_kind` 决定匹配命令内容还是工具名称
4. **向后兼容**: 不传参时行为完全不变

---

## 一、接口参数变更

### 1.1 新增参数位置

三个接口的请求体中，`agent_config.agent_server` 下新增 `tool_approval_rules` 字段：

```
POST /chat
POST /computer/chat
POST /devcomputer/chat

请求体路径: agent_config.agent_server.tool_approval_rules
```

### 1.2 参数结构

#### `tool_approval_rules` (可选)

类型: `ToolApprovalRule[] | null`

- 不传或 `null`: `agent_mode` 决定默认行为（完全不变）
- 空数组 `[]`: 同上
- 非空数组: 按规则匹配

每条规则包含以下字段：

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `patterns` | `string[]` | 是 | - | 正则表达式列表（大小写不敏感，任一命中即触发，空数组则该规则不匹配任何工具） |
| `action` | `string` | 是 | - | `"require_approval"` 或 `"auto_approve"` |
| `tool_kind` | `string` | 否 | `"Execute"` | ACP ToolKind 过滤，决定匹配目标 |

#### `action` 取值说明

| 值 | 说明 |
|----|------|
| `"require_approval"` | 命中时强制要求用户审批（即使在 YOLO 模式下） |
| `"auto_approve"` | 命中时自动放行（即使在 ASK 模式下） |

#### `tool_kind` 取值与匹配目标

`tool_kind` 对应 ACP 协议的 `ToolKind` 枚举，决定正则表达式匹配什么内容：

| `tool_kind` | 匹配目标 | 说明 |
|-------------|---------|------|
| `"Execute"`（默认） | `tool_call.raw_input.command` | Bash/Shell 命令内容 |
| `"Read"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Edit"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Delete"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Move"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Search"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Think"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Fetch"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"SwitchMode"` | `tool_call.raw_input.tool_name` | 工具名称 |
| `"Other"` | `tool_call.raw_input.tool_name` | 工具名称 |

**设计原理**：
- `Execute` 类型的工具（bash/shell），用户关心的是**执行什么命令**，所以匹配命令内容
- 其他类型的工具，用户关心的是**调用什么工具**，所以匹配工具名称

---

## 二、入参示例

### 场景 1: YOLO 模式 + Bash 危险命令审批

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "帮我删除临时文件",
  "agent_config": {
    "agent_server": {
      "agent_mode": "yolo",
      "tool_approval_rules": [
        {
          "patterns": ["rm\\s+-[a-z]*r[a-z]*f", "sudo", "chmod\\s+777"],
          "action": "require_approval"
        }
      ]
    }
  }
}
```

> `tool_kind` 不传 → 默认 `"Execute"` → 匹配 `raw_input.command`

### 场景 2: ASK 模式 + 只读命令自动放行

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "帮我查看代码",
  "agent_config": {
    "agent_server": {
      "agent_mode": "ask",
      "tool_approval_rules": [
        {
          "patterns": ["^(ls|cat|head|tail|grep|find|wc|echo|pwd|git\\s+status|git\\s+log)\\b"],
          "action": "auto_approve"
        }
      ]
    }
  }
}
```

### 场景 3: YOLO 模式 + 混合（Bash 审批 + MCP Delete 审批）

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "帮我操作数据库",
  "agent_config": {
    "agent_server": {
      "agent_mode": "yolo",
      "tool_approval_rules": [
        {
          "patterns": ["rm\\s+-rf", "sudo"],
          "action": "require_approval"
        },
        {
          "patterns": [".*delete.*", ".*drop.*", ".*truncate.*"],
          "action": "require_approval",
          "tool_kind": "Delete"
        }
      ]
    }
  }
}
```

> 第 1 条: `tool_kind=Execute`(默认) → 匹配命令内容中的 `rm -rf` / `sudo`
> 第 2 条: `tool_kind=Delete` → 匹配工具名称中的 `delete` / `drop` / `truncate`

### 场景 4: ASK 模式 + 混合（Read 放行 + Edit 审批）

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "帮我重构代码",
  "agent_config": {
    "agent_server": {
      "agent_mode": "ask",
      "tool_approval_rules": [
        {
          "patterns": ["^(ls|cat|head|tail|grep|find|git\\s+status)\\b"],
          "action": "auto_approve"
        },
        {
          "patterns": [".*read.*", ".*list.*", ".*get.*", ".*search.*"],
          "action": "auto_approve",
          "tool_kind": "Read"
        },
        {
          "patterns": [".*write.*", ".*edit.*", ".*delete.*"],
          "action": "require_approval",
          "tool_kind": "Edit"
        }
      ]
    }
  }
}
```

> 第 1 条: Execute 类型 bash 命令自动放行（ls/cat/grep 等）
> 第 2 条: Read 类型 MCP 工具自动放行
> 第 3 条: Edit 类型 MCP 工具强制审批

### 场景 5: 全部 Delete 类型工具审批（不限工具名称）

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "帮我清理文件",
  "agent_config": {
    "agent_server": {
      "agent_mode": "yolo",
      "tool_approval_rules": [
        {
          "patterns": [".*"],
          "action": "require_approval",
          "tool_kind": "Delete"
        }
      ]
    }
  }
}
```

### 场景 6: 不传规则，行为不变

```json
{
  "user_id": "user_123",
  "project_id": "proj_456",
  "prompt": "hello",
  "agent_config": {
    "agent_server": {
      "agent_mode": "yolo"
    }
  }
}
```

---

## 三、决策流程

### 3.1 优先级链

```
收到 Agent 的权限审批请求 (RequestPermissionRequest)
    │
    ▼
① 用户保存的规则检查 (RuleStore，之前审批时保存的规则)
    │ 命中 Deny → 拒绝
    │ 命中 Allow → 放行
    │ 未命中 ↓
    │
② tool_approval_rules 规则匹配（按数组顺序，首条命中即停）
    │ 命中 require_approval → 强制审批（SSE 推送前端）
    │ 命中 auto_approve → 自动放行
    │ 未命中 ↓
    │
③ agent_mode 默认行为
    │ yolo → 自动放行
    │ ask → 审批（SSE 推送前端）
```

### 3.2 匹配逻辑

对于 `tool_approval_rules` 中的每条规则，按以下步骤匹配：

1. **检查 tool_kind 是否匹配**: 将规则的 `tool_kind`（默认 `"Execute"`）与 Agent 请求中的 `tool_call.kind` 比较，不匹配则跳过此规则
2. **提取匹配目标**:
   - `tool_kind == "Execute"` → 取 `tool_call.raw_input.command`
   - 其他 → 取 `tool_call.raw_input.tool_name`（回退: `toolName` → `title` 首词 → `"tool"`）
3. **正则匹配**: 用 `patterns` 中的每个正则（大小写不敏感）匹配目标字符串，任一命中即触发
4. **返回 action**: 首条命中规则的 `action` 决定行为

### 3.3 匹配示例

#### Execute 类型（匹配命令内容）

| `action` | `patterns` | 命令内容 | 结果 |
|----------|-----------|---------|------|
| `require_approval` | `["rm\\s+-rf"]` | `rm -rf /tmp/cache` | 强制审批 |
| `require_approval` | `["rm\\s+-rf"]` | `rm -f /tmp/file` | 不匹配 |
| `auto_approve` | `["^(ls\|cat)\\b"]` | `ls -la` | 自动放行 |
| `auto_approve` | `["^(ls\|cat)\\b"]` | `rm -rf /` | 不匹配 |

#### Delete 类型（匹配工具名称）

| `action` | `patterns` | 工具名称 | 结果 |
|----------|-----------|---------|------|
| `require_approval` | `[".*delete.*"]` | `file_delete` | 强制审批 |
| `require_approval` | `[".*delete.*"]` | `read_file` | 不匹配 |
| `require_approval` | `[".*"]` | `any_tool` | 强制审批 |

#### Read 类型（匹配工具名称）

| `action` | `patterns` | 工具名称 | 结果 |
|----------|-----------|---------|------|
| `auto_approve` | `[".*read.*", ".*list.*"]` | `mcp__server__list_items` | 自动放行 |
| `auto_approve` | `[".*read.*", ".*list.*"]` | `mcp__server__delete_item` | 不匹配 |

---

## 四、SSE 事件行为（前端关注）

### 4.1 事件推送

当规则命中 `require_approval` 时，SSE 推送行为与 ASK 模式**完全一致**：

- **事件类型**: `AcpRequestPermission`
- **数据结构**: 与现有 ASK 模式的 SSE 事件相同
- **前端无需区分**: 收到 `AcpRequestPermission` 事件就展示审批 UI

### 4.2 审批结果回传

审批结果回传接口 `/computer/notify-resolved` **无需修改**，现有的回传逻辑完全适用。

---

## 五、向后兼容性

| 场景 | 行为 |
|------|------|
| 不传 `tool_approval_rules` | `agent_mode` 决定默认行为（完全不变） |
| `tool_approval_rules = []` | 空数组，`agent_mode` 决定默认行为 |
| 规则都不匹配 | `agent_mode` 决定默认行为 |
| YOLO + 命中 `require_approval` | 强制审批 |
| YOLO + 命中 `auto_approve` | 自动放行（与默认一致） |
| ASK + 命中 `auto_approve` | 自动放行 |
| ASK + 命中 `require_approval` | 强制审批（与默认一致） |

---

## 六、错误处理

| 场景 | 处理方式 |
|------|----------|
| `action` 值不合法 | 返回参数校验错误 |
| `tool_kind` 值不合法 | 返回参数校验错误 |
| `patterns` 中包含空字符串 | 忽略空字符串，继续匹配其他 pattern |
| `patterns` 中的正则表达式无效 | 该 pattern 匹配失败，继续匹配其他 pattern |
| `patterns` 为空数组 | 该规则不匹配任何工具 |
| 工具调用的 `kind` 为空 | 当作 `"Other"` 处理 |

---

## 七、安全考虑

### 7.1 容器环境下的审批策略

所有命令在容器内执行，不存在需要自动拒绝的"危险命令"。`rm -rf /` 等操作在容器内只影响容器自身，不会影响宿主机。因此：
- 不设置硬编码安全规则自动拒绝
- 所有命令的审批与否完全由 `tool_approval_rules` 和 `agent_mode` 决定
- 用户可以通过 `require_approval` 规则对敏感操作进行人工审批

### 7.2 ASK 模式下 auto_approve 的安全边界

- 建议只对只读/低风险工具配置 `auto_approve`
- 敏感操作（删除、写入、执行等）建议保持默认审批或配置 `require_approval`

---

## 八、ACP 协议字段参考

用于匹配的字段来自 ACP 协议的 `RequestPermissionRequest`（`agent-client-protocol-schema` v0.12.0）：

```
RequestPermissionRequest
├── session_id: string
├── tool_call: ToolCallUpdate
│   ├── tool_call_id: string
│   ├── kind: string | null          ← 工具类型（Read/Edit/Execute/Delete/...）
│   ├── title: string | null         ← 人类可读标题（如 "bash - ls"）
│   ├── content: array | null
│   ├── locations: array | null
│   ├── raw_input: object | null     ← 原始输入（command / tool_name 等）
│   └── raw_output: object | null
├── options: PermissionOption[]
└── _meta: object | null
```

**`kind` 枚举值**（ACP 协议提供）：

```
Read | Edit | Delete | Move | Search | Execute | Think | Fetch | SwitchMode | Other
```

> **注意**: `kind` 是按**操作类型**分类的，不能区分 Bash 工具和 MCP 工具。
> 例如 `Execute` 可能是 bash `rm -rf`，也可能是 MCP 的 `run_query`。
> 但 `kind` 足以决定**匹配什么内容**：`Execute` 匹配命令内容，其他匹配工具名称。

**字段提取规则**：

| 匹配场景 | 提取字段 | 备选字段 |
|---------|---------|---------|
| 工具类型过滤 | `tool_call.kind` | 为空时当作 `"Other"` |
| Execute 匹配目标 | `tool_call.raw_input.command` | - |
| 其他类型匹配目标 | `tool_call.raw_input.tool_name` | `raw_input.toolName` → `title` 首词 → `"tool"` |

---

## 附录 A：后端实现参考

### A.1 后端文件修改清单

| 文件 | 修改内容 |
|------|----------|
| `crates/shared_types/src/chat_agent_config.rs` | 新增 `ToolApprovalRule`、`ToolApprovalAction` 类型；`ChatAgentServerConfig` 新增 `tool_approval_rules` 字段 |
| `crates/agent_abstraction/src/traits/permission_handler.rs` | `PermissionRequestContext` 新增 `tool_approval_rules` 字段 |
| `crates/agent_abstraction/src/traits/agent.rs` | `AgentStartConfig` 新增 `tool_approval_rules` 字段和 builder 方法 |
| `crates/agent_abstraction/src/client/builder.rs` | `AcpClientBuilder` 新增 `tool_approval_rules` 字段和 setter |
| `crates/agent_abstraction/src/session/acp_worker.rs` | 传播 `tool_approval_rules` 到 `AgentStartConfig` |
| `crates/agent_abstraction/src/launcher/claude_code_sacp/connection.rs` | 构建 `PermissionRequestContext` 时传入 `tool_approval_rules` |
| `crates/agent_runner/src/grpc/conversion.rs` | 校验 `action`、`tool_kind` 合法性，透传配置 |
| `crates/agent_runner/src/service/permission_manager.rs` | 新增匹配逻辑，修改决策流程 |

### A.2 数据结构定义

```rust
/// 工具审批规则
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolApprovalRule {
    /// 正则表达式列表（大小写不敏感，任一命中即触发，OR 逻辑）
    pub patterns: Vec<String>,
    /// 审批动作: "require_approval" | "auto_approve"
    pub action: ToolApprovalAction,
    /// ACP ToolKind 过滤（可选），不传默认 "Execute"
    #[serde(default)]
    pub tool_kind: Option<String>,
}

/// 审批动作枚举
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalAction {
    RequireApproval,
    AutoApprove,
}
```

### A.3 匹配逻辑伪代码

```
function match_approval_rules(request, rules):
    for rule in rules:
        rule_tool_kind = rule.tool_kind ?? "Execute"

        // ① 检查 tool_kind 是否匹配
        if request.tool_call.kind != rule_tool_kind:
            continue

        // ② 根据 tool_kind 提取匹配目标
        if rule_tool_kind == "Execute":
            target = request.tool_call.raw_input.command ?? ""
        else:
            target = request.tool_call.raw_input.tool_name
                 ?? request.tool_call.raw_input.toolName
                 ?? first_word(request.tool_call.title)
                 ?? "tool"

        // ③ 正则匹配（大小写不敏感）
        for pattern in rule.patterns:
            if regex_match(pattern, target, case_insensitive=true):
                return rule.action

    return null  // 无匹配，按 agent_mode 默认行为
```

### A.4 后端测试用例

```bash
# 测试 1: YOLO + Bash 危险命令审批
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user",
    "project_id": "test_project",
    "prompt": "删除临时文件",
    "agent_config": {
      "agent_server": {
        "agent_mode": "yolo",
        "tool_approval_rules": [
          { "patterns": ["rm\\s+-[a-z]*r[a-z]*f", "sudo"], "action": "require_approval" }
        ]
      }
    }'
# 预期: "rm -rf /tmp" 触发审批，"ls" 自动放行

# 测试 2: ASK + 只读命令自动放行
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user",
    "project_id": "test_project",
    "prompt": "查看代码",
    "agent_config": {
      "agent_server": {
        "agent_mode": "ask",
        "tool_approval_rules": [
          { "patterns": ["^(ls|cat|head|tail|grep|find)\\b"], "action": "auto_approve" }
        ]
      }
    }'
# 预期: "ls -la" 自动放行，"rm -rf" 触发审批

# 测试 3: 不传规则 → 行为不变
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user",
    "project_id": "test_project",
    "prompt": "hello",
    "agent_config": { "agent_server": { "agent_mode": "yolo" } }
  }'
# 预期: 所有工具自动放行
```
