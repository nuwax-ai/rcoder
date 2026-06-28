# tool_approval_rules 工具审批规则匹配规范

> 本文档定义 `tool_approval_rules` 的匹配标准。**rcoder 后端与各客户端（Electron / nuwaclaw 等）必须共同遵守**，以保证同一规则在不同端产生一致的审批决策。

## 1. 背景与目标

`tool_approval_rules` 用于在不依赖 `agent_mode` 默认行为的前提下，对特定工具调用精细控制审批（`ask` / `allow` / `deny`）。

不同 ACP agent（nuwaxcode / claude-code-acp-ts / 其他）上报的 `tool_call` 结构存在差异。下方「真实性」列基于 **nuwaxcode 客户端日志 `electron-dev.log`** 与 **rcoder `startup.log`** 实际核对：
- 命令内容：bash 工具用 `raw_input.command`（✅ 已验证）；`cmd`/`script` 为客户端 `extractCommandValue` 约定的防御性别名（nuwaxcode 未用）；
- 工具名：nuwaxcode MCP 工具用 `raw_input.tool`（✅ 已验证，如 `{"tool":"get_stock_data"}`）+ `title`（✅ 必现，如 `A_get_stock_data`）；`tool_name`/`toolName` 为防御性别名（nuwaxcode 未用）；
- `kind`：nuwaxcode MCP 工具为 `other`（ACP `#[serde(other)]` 兜底）。

为避免「某 agent 把信息放在非预期字段导致漏审」，本规范采用**容错式多字段匹配**：通用规则下把所有身份类字段都纳入，任一命中即触发。

## 2. 数据结构

```rust
struct ToolApprovalRule {
    patterns: Vec<String>,          // glob 通配符列表（大小写不敏感，OR 逻辑）
    action: ToolApprovalAction,     // Ask | Allow | Deny
    tool_kind: Option<String>,      // None=通用规则；Some(x)=仅匹配 kind==x
}
```

## 3. 匹配字段集

> **协议依据**（ACP SDK `agent-client-protocol-schema` v1.1.0）：`tool_call` 的协议标准字段为 `kind` / `title` / `status` / `content` / `locations` / `raw_input` / `raw_output`（struct 字段，稳定可移植）；其中 `raw_input` 类型是 `Option<serde_json::Value>`——**任意 JSON，协议不规定内部 key**。因此下表里 `command` / `tool` / `tool_name` 等 **raw_input 内部 key 都是各 agent 自定义约定**（协议不保证存在），这正是匹配逻辑必须「多字段容错」的根本原因。

通用规则（`tool_kind: None`）从 `RequestPermissionRequest.tool_call` 收集以下字段作为匹配目标，**去重、跳过空值**：

| 字段 | 来源 | 纳入 | 真实性 | 理由 |
|------|------|------|--------|------|
| `command` | `raw_input.command` | ✅ | ✅ 已验证（startup.log bash） | Execute/bash 命令内容，核心 |
| `cmd` | `raw_input.cmd` | ✅ | ⚠️ 防御（客户端约定，nuwaxcode 未用） | 命令别名 |
| `script` | `raw_input.script` | ✅ | ⚠️ 防御（客户端约定，nuwaxcode 未用） | 命令别名 |
| 字符串 `raw_input` | `raw_input` 本身为 string | ✅ | ⚠️ 防御 | 整体视为命令 |
| `tool` | `raw_input.tool` | ✅ | ✅ 已验证（electron-dev.log MCP） | **nuwaxcode MCP 工具名 key** |
| `tool_name` | `raw_input.tool_name` | ✅ | ⚠️ 防御（nuwaxcode 未用） | 工具名别名 |
| `toolName` | `raw_input.toolName` | ✅ | ⚠️ 防御（nuwaxcode 未用） | 工具名驼峰别名 |
| `title` 首词 | `tool_call.title` 首 token | ✅ | ✅ 已验证（必现） | 通常 = 工具名 |
| `title` 完整 | `tool_call.title` | ✅ | ✅ 已验证（必现） | 最通用兜底 |
| `kind` | `tool_call.kind` | ❌ | — | 枚举分类，只用于过滤 |
| `tool_call_id` | — | ❌ | — | 无语义标识（`call_00_xxx`） |
| `content` / `locations` / `raw_output` | — | ❌ | — | 结果/元数据，非身份 |
| ~~`input/command` pointer~~ | ~~`raw_input` JSON pointer~~ | ❌ 已移除 | ✗ 臆测 | `input` 是客户端日志字段名，非 raw_input 嵌套；已删除 |

**设计原则**：身份类字段全纳入（鲁棒，不赌 agent 把信息放哪个字段）；非身份字段严格排除（避免 `tool_call_id` 等无意义串误命中）。

## 4. 匹配逻辑（双路径）

```text
match_tool_approval_rules(request, rules):
    actual_kind = request.tool_call.kind ?? "Other"

    for rule in rules:                              # 首条命中即停
        # ① kind 过滤
        if rule.tool_kind != null
           and actual_kind != rule.tool_kind (大小写不敏感):
            continue

        # ② 选目标
        targets = (rule.tool_kind == null)
            ? extract_all_targets(request)                              # 通用规则 → 多字段（第 3 节）
            : [extract_target_by_kind(request, rule.tool_kind)]         # 显式 → 单字段
              # 命令类 kind → command 族首个非空
              # 其他 kind   → tool_name 族首个非空，兜底 "tool"

        targets = dedup(targets).filter(nonEmpty)
        if targets is empty: continue

        # ③ 匹配：任一 pattern × 任一 target 命中 → 规则命中
        for pattern in rule.patterns:
            if pattern.trim() == "": continue
            if any(glob_match(pattern, t) for t in targets):
                return rule.action

    return null   # 无命中，回退 agent_mode 默认
```

### 命令类 kind 判定

`is_command_like_kind = { execute, bash, terminal, shell, command }`（小写比较）：
- `execute` 为 ACP 标准；
- 其余兼容部分 agent 的自定义 kind 命名。

显式 `tool_kind` 命中命令类时，匹配目标取 **command 族**（`command`/`cmd`/`script`/字符串 rawInput）首个非空；否则取 **tool_name 族**（`tool`/`tool_name`/`toolName`/`title` 首词）首个非空，兜底 `"tool"`。

## 5. 匹配语义

| 维度 | 语义 |
|------|------|
| 多 patterns | **OR**（任一命中） |
| 多字段（通用规则） | **OR**（任一字段命中） |
| 多 rules | **顺序优先**（首条命中即返回，后续不评估） |
| 大小写 | pattern 与 kind 比较均**不敏感** |

## 6. glob 通配规范

| 语法 | 支持 | 说明 |
|------|------|------|
| `*` | ✅ | 任意数量字符（含 `/`） |
| `?` | ✅ | 单字符 |
| `[abc]` / `[a-z]` / `[!abc]` | ✅ | 字符类 / 范围 / 取反 |
| 锚定 | ✅ | 全匹配（`^...$`），非子串 |
| 大小写 | 不敏感 | — |
| `{a,b}` brace | ❌ | 不支持，避免歧义 |
| `**` | ❌ | 不支持（目录语义与工具名无关） |
| 空 pattern | 跳过 | 不匹配 |
| 非法 pattern | 返回不命中 | 不抛错 |

- **rcoder**：`globset::GlobBuilder::new(pattern).case_insensitive(true)`（默认 `*` 含 `/`、不支持 brace，符合规范）。
- **客户端**：手写 globToRegex 需对齐上表（`*`→`.*` 含 `/`，禁 brace / `**`）。

## 7. 在权限决策链中的位置

```text
① 危险命令仅记录日志（仅 command；warn 日志，不拦截、不强制审批、不 deny；不影响后续决策）
② 用户保存的规则（RuleStore，前端勾「总是允许/拒绝」持久化）
③ tool_approval_rules（本规范）        ← 首条命中即停
④ agent_mode 兜底（yolo = 放行，ask = 推前端）
```

- 危险命令检测（①）只对 command 生效，命中后**仅打 warn 日志（观测/审计），不拦截、不改变决策**——审批完全由 ②③④ 决定，与容器环境「不自动拒绝危险命令、审批完全配置驱动」的立场一致。
- 如需对危险命令强制审批，用户可在 `tool_approval_rules` 配置（如 `rm -rf * → ask`）。

## 8. 实现对齐

| 端 | 实现位置 | 要求 |
|----|---------|------|
| **rcoder** | `crates/agent_runner/src/service/permission_manager.rs` 的 `match_tool_approval_rules` | 双路径 + 多字段（`extract_all_targets` / `extract_target_by_kind`）+ globset |
| **客户端** | 各端 permission 模块（nuwaclaw: `toolApprovalRules.ts`） | 字段集与第 3 节一致；glob 语义与第 6 节一致；kind 过滤大小写不敏感 |

两边行为应可互验：同一 `(request, rules)` 输入，两端 `match_tool_approval_rules` 返回相同 action。

## 9. 示例

### 通用规则（多字段任一命中）
```json
{ "patterns": ["*get_stock_data", "rm -rf *"], "action": "ask" }
```
- MCP 工具 `get_stock_data`（kind=Other，工具名在 title）→ 命中工具名 → ask
- bash `rm -rf /tmp`（kind=Execute）→ 命中 command → ask
- bash `ls`（kind=Execute）→ 不命中任何字段 → 回退 agent_mode

### 显式 tool_kind（精确单字段）
```json
{ "patterns": ["*delete*"], "action": "deny", "tool_kind": "Delete" }
```
- 仅匹配 kind=Delete 的工具，目标取工具名首个非空。

### ask 模式下放行只读命令
```json
{ "patterns": ["ls *", "cat *", "grep *"], "action": "allow" }
```
- 通用规则，命中 command（Execute 工具）→ allow 自动放行。

## 10. 取舍说明

- **鲁棒 vs 精确**：通用规则（多字段）偏鲁棒，可能误命中（如某 pattern 恰好出现在 title/tool_name）。追求精确的场景用显式 `tool_kind` 收紧到单字段。
- **危险命令仅记录日志**：`rm -rf /` 等极端命令会打 warn 日志（观测），但**不拦截、不改变决策**——审批完全由 `tool_approval_rules` 和 `agent_mode` 决定；如需强制审批，配 `rm -rf * → ask`。
