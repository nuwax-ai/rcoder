# ACP会话架构

<cite>
**本文档中引用的文件**   
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs)
- [connection.rs](file://crates/acp_thread/src/connection.rs)
- [diff.rs](file://crates/acp_thread/src/diff.rs)
- [mention.rs](file://crates/acp_thread/src/mention.rs)
- [terminal.rs](file://crates/acp_thread/src/terminal.rs)
</cite>

## 目录
1. [引言](#引言)
2. [项目结构](#项目结构)
3. [核心组件](#核心组件)
4. [会话状态机实现](#会话状态机实现)
5. [网络传输层抽象](#网络传输层抽象)
6. [终端环境模拟](#终端环境模拟)
7. [增量更新与提及解析](#增量更新与提及解析)
8. [消息格式与序列化](#消息格式与序列化)
9. [长连接保活与错误恢复](#长连接保活与错误恢复)
10. [结论](#结论)

## 引言
ACP（Agent Client Protocol）会话层架构设计旨在实现客户端与AI代理之间的高效双向通信。本架构通过acp_thread crate维护会话状态，支持复杂的交互式命令执行场景。系统采用分层设计，将网络传输、终端模拟、内容处理等职责分离，确保了系统的可维护性和扩展性。

## 项目结构
acp_thread crate包含多个核心模块，分别处理会话的不同方面。这种模块化设计使得各功能组件可以独立演进，同时保持良好的集成性。

```mermaid
graph TD
A[acp_thread] --> B[acp_thread.rs]
A --> C[connection.rs]
A --> D[diff.rs]
A --> E[mention.rs]
A --> F[terminal.rs]
B --> G[会话状态管理]
C --> H[网络连接抽象]
D --> I[差异比较]
E --> J[提及解析]
F --> K[终端模拟]
```

**图示来源**
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs)
- [connection.rs](file://crates/acp_thread/src/connection.rs)
- [diff.rs](file://crates/acp_thread/src/diff.rs)
- [mention.rs](file://crates/acp_thread/src/mention.rs)
- [terminal.rs](file://crates/acp_thread/src/terminal.rs)

## 核心组件
acp_thread crate的核心组件包括会话管理、连接抽象、终端模拟和内容处理等模块。这些组件协同工作，实现了完整的ACP会话功能。

**组件来源**
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs#L775-L800)
- [connection.rs](file://crates/acp_thread/src/connection.rs#L21-L50)
- [terminal.rs](file://crates/acp_thread/src/terminal.rs#L10-L20)

## 会话状态机实现
acp_thread.rs实现了ACP会话的状态机，管理从连接建立到消息分发的完整生命周期。状态机通过事件驱动的方式处理各种会话事件，确保状态转换的正确性和一致性。

```mermaid
stateDiagram-v2
[*] --> Idle
Idle --> Connecting : new_thread
Connecting --> Connected : connection_established
Connected --> MessageProcessing : prompt_received
MessageProcessing --> AuthorizationPending : tool_call_requires_confirmation
AuthorizationPending --> MessageProcessing : authorization_granted
MessageProcessing --> Connected : processing_complete
Connected --> Idle : session_closed
Any --> Error : error_occurred
Error --> Idle : cleanup
```

**图示来源**
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs#L775-L800)
- [connection.rs](file://crates/acp_thread/src/connection.rs#L21-L50)

## 网络传输层抽象
connection.rs通过AgentConnection trait抽象了网络传输层，提供了统一的接口来处理不同类型的连接。这种抽象使得上层逻辑无需关心具体的网络实现细节。

```mermaid
classDiagram
class AgentConnection {
+new_thread(project, cwd, cx) Task<Result<Entity<AcpThread>>>
+auth_methods() &[acp : : AuthMethod]
+authenticate(method, cx) Task<Result<()>>
+prompt(user_message_id, params, cx) Task<Result<acp : : PromptResponse>>
+cancel(session_id, cx) void
}
class AgentSessionTruncate {
+run(message_id, cx) Task<Result<()>>
}
class AgentSessionResume {
+run(cx) Task<Result<acp : : PromptResponse>>
}
class AgentSessionSetTitle {
+run(title, cx) Task<Result<()>>
}
AgentConnection <|-- StubAgentConnection
AgentConnection <|-- FakeAgentConnection
AgentConnection --> AgentSessionTruncate
AgentConnection --> AgentSessionResume
AgentConnection --> AgentSessionSetTitle
```

**图示来源**
- [connection.rs](file://crates/acp_thread/src/connection.rs#L21-L150)

## 终端环境模拟
terminal.rs模块实现了终端环境的模拟，支持交互式命令执行。通过封装底层终端操作，提供了安全的命令执行环境和输出处理机制。

```mermaid
sequenceDiagram
participant Client as "客户端"
participant Terminal as "Terminal"
participant PTY as "PTY进程"
Client->>Terminal : execute_command(command)
Terminal->>PTY : spawn_process(command)
loop 输出处理
PTY-->>Terminal : 输出数据
Terminal->>Terminal : 应用字节限制
Terminal->>Terminal : 更新输出状态
end
PTY-->>Terminal : 进程结束
Terminal->>Terminal : 收集退出状态
Terminal-->>Client : 返回执行结果
```

**图示来源**
- [terminal.rs](file://crates/acp_thread/src/terminal.rs#L10-L50)

## 增量更新与提及解析
diff.rs和mention.rs模块分别处理增量更新和提及解析的特殊逻辑。这些功能增强了会话的表达能力和交互性。

### 增量更新处理
```mermaid
flowchart TD
A[接收到Diff更新] --> B{是否需要更新?}
B --> |是| C[创建新的BufferDiff]
B --> |否| D[保持现有状态]
C --> E[更新MultiBuffer]
E --> F[通知UI更新]
F --> G[完成更新]
```

**图示来源**
- [diff.rs](file://crates/acp_thread/src/diff.rs#L10-L100)

### 提及解析逻辑
```mermaid
flowchart TD
A[解析提及URI] --> B{scheme类型}
B --> |file| C[处理文件路径]
B --> |zed| D[处理Zed特定URL]
B --> |http/https| E[处理网络资源]
C --> F[提取路径和行号]
D --> G[解析会话或规则ID]
E --> H[验证URL有效性]
F --> I[创建MentionUri]
G --> I
H --> I
I --> J[返回解析结果]
```

**图示来源**
- [mention.rs](file://crates/acp_thread/src/mention.rs#L25-L100)

## 消息格式与序列化
ACP会话采用结构化的消息格式，通过serde进行序列化和反序列化。消息格式设计考虑了扩展性和兼容性，支持多种内容类型。

```mermaid
erDiagram
USER_MESSAGE {
string id PK
string content
array chunks
object checkpoint
}
ASSISTANT_MESSAGE {
array chunks
}
TOOL_CALL {
string id PK
string label
enum kind
array content
enum status
array locations
}
CONTENT_BLOCK {
enum type
string content
}
DIFF {
string path
string old_text
string new_text
}
TERMINAL {
string id
string command
object working_dir
}
USER_MESSAGE ||--o{ CONTENT_BLOCK : 包含
ASSISTANT_MESSAGE ||--o{ CONTENT_BLOCK : 包含
TOOL_CALL ||--o{ CONTENT_BLOCK : 包含
TOOL_CALL ||--o{ DIFF : 包含
TOOL_CALL ||--o{ TERMINAL : 包含
```

**图示来源**
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs#L40-L200)
- [diff.rs](file://crates/acp_thread/src/diff.rs#L10-L20)
- [terminal.rs](file://crates/acp_thread/src/terminal.rs#L10-L20)

## 长连接保活与错误恢复
系统实现了长连接保活策略和错误恢复机制，确保会话的稳定性和可靠性。通过心跳检测和自动重连，最大限度地减少连接中断的影响。

```mermaid
sequenceDiagram
participant Client as "客户端"
participant Server as "服务器"
participant Monitor as "连接监控"
loop 心跳检测
Monitor->>Client : check_connection()
alt 连接正常
Client->>Server : send_heartbeat()
Server-->>Client : heartbeat_response()
Monitor->>Monitor : reset_timeout()
else 连接异常
Monitor->>Client : trigger_reconnect()
Client->>Server : establish_new_connection()
Server-->>Client : connection_established()
Client->>Client : restore_session_state()
Monitor->>Monitor : connection_restored()
end
end
```

**图示来源**
- [connection.rs](file://crates/acp_thread/src/connection.rs#L21-L50)
- [acp_thread.rs](file://crates/acp_thread/src/acp_thread.rs#L775-L800)

## 结论
ACP会话架构通过清晰的分层设计和模块化实现，提供了稳定可靠的双向通信通道。各组件职责明确，接口定义清晰，为后续功能扩展和性能优化奠定了良好基础。系统的错误恢复机制和长连接保活策略确保了在复杂网络环境下的稳定性，而丰富的消息格式支持则满足了多样化的交互需求。