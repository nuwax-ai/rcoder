# 上下文感知工具API

<cite>
**本文档中引用的文件**  
- [diagnostics_tool.rs](file://crates/agent2/src/tools/diagnostics_tool.rs)
- [open_tool.rs](file://crates/agent2/src/tools/open_tool.rs)
- [now_tool.rs](file://crates/agent2/src/tools/now_tool.rs)
- [thinking_tool.rs](file://crates/agent2/src/tools/thinking_tool.rs)
- [history_store.rs](file://crates/agent2/src/history_store.rs)
- [thread.rs](file://crates/agent2/src/thread.rs)
</cite>

## 目录
1. [简介](#简介)
2. [核心工具功能](#核心工具功能)
3. [诊断信息获取工具 (diagnostics_tool)](#诊断信息获取工具-diagnostics_tool)
4. [文件与URL打开工具 (open_tool)](#文件与url打开工具-open_tool)
5. [时间上下文工具 (now_tool)](#时间上下文工具-now_tool)
6. [思维过程可视化工具 (thinking_tool)](#思维过程可视化工具-thinking_tool)
7. [历史记录与调用轨迹管理](#历史记录与调用轨迹管理)
8. [审计日志与查询接口](#审计日志与查询接口)
9. [结论](#结论)

## 简介
本文档详细描述了上下文感知类工具的API设计与功能实现，涵盖诊断信息获取、文件打开、时间查询和思维过程可视化等核心功能。这些工具作为智能代理（agent）系统的重要组成部分，旨在增强调试能力、提升可解释性，并为用户提供透明的操作反馈。文档将深入分析各工具的API端点、使用场景及其在系统中的集成方式。

## 核心工具功能
上下文感知工具集为智能代理提供了与外部环境交互的能力，包括读取项目状态、执行系统操作、获取实时信息以及暴露内部推理过程。这些工具通过标准化的API接口进行调用，并与`agent2`模块中的`history_store`和`thread`组件紧密集成，确保所有操作均可追溯和审计。

## 诊断信息获取工具 (diagnostics_tool)

`diagnostics_tool`用于收集项目环境中的诊断信息，如LSP（语言服务器协议）状态和Git状态。该工具能够返回特定文件的错误和警告，或提供整个项目的诊断摘要。

当提供路径时，工具会检查该文件的所有诊断信息；若未提供路径，则返回项目范围内所有文件的错误和警告计数汇总。此功能在用户请求修复代码问题或评估代码质量时尤为有用。

**Section sources**
- [diagnostics_tool.rs](file://crates/agent2/src/tools/diagnostics_tool.rs#L1-L167)

## 文件与URL打开工具 (open_tool)

`open_tool`允许代理触发文件跳转或UI操作，通过操作系统的默认应用程序打开指定的文件或URL。其行为根据平台有所不同：
- 在macOS上等效于`open`命令
- 在Windows上等效于`start`命令
- 在Linux上使用`xdg-open`、`gio open`等适配命令

该工具仅应在用户明确请求时使用，不得擅自调用。它支持打开本地文件（如PDF）或远程资源（如网页链接），从而实现无缝的跨应用导航。

**Section sources**
- [open_tool.rs](file://crates/agent2/src/tools/open_tool.rs#L1-L170)

## 时间上下文工具 (now_tool)

`now_tool`提供当前时间的上下文信息，返回符合RFC 3339格式的日期时间字符串。用户可选择使用UTC或本地时区。

该工具仅在任务需要知晓当前时间时才应被调用，例如生成时间戳、安排任务或处理与时间相关的逻辑。返回值包含完整的日期、时间和时区信息，确保时间上下文的准确性和一致性。

**Section sources**
- [now_tool.rs](file://crates/agent2/src/tools/now_tool.rs#L1-L64)

## 思维过程可视化工具 (thinking_tool)

`thinking_tool`用于暴露AI代理的内部推理链，支持问题分析、策略制定和方案规划等非执行性思考过程。通过此工具，代理可以逐步展示其解决问题的思路，增强决策过程的透明度。

调用时需提供待思考的内容描述，工具会将该内容作为“思考”片段记录在对话流中，供后续参考和审查。此机制不仅提升了可解释性，也为用户理解代理行为提供了直观依据。

**Section sources**
- [thinking_tool.rs](file://crates/agent2/src/tools/thinking_tool.rs#L1-L52)

## 历史记录与调用轨迹管理

工具调用与`agent2`中的`history_store`和`thread`模块密切相关，所有操作均被记录并可回溯。

`HistoryStore`负责管理会话历史，维护`AcpThread`和文本线程的元数据。每次工具调用都会更新线程状态，并通过`DbThread`结构持久化相关信息，包括消息序列、更新时间、摘要和令牌使用情况。

`Thread`模块则负责处理完整的交互流程，包括用户消息、代理响应及工具调用事件的编排。每个工具调用被视为一个独立的`ToolCall`事件，其状态变化（如授权、执行、完成或失败）均被记录在`ToolCallUpdate`中，确保整个生命周期的可观测性。

**Section sources**
- [history_store.rs](file://crates/agent2/src/history_store.rs#L1-L357)
- [thread.rs](file://crates/agent2/src/thread.rs#L1-L799)

## 审计日志与查询接口

系统提供完整的审计日志功能，支持对工具调用的全面追踪。每条日志记录包含以下关键信息：
- **调用者身份**：发起调用的会话ID（`SessionId`）和用户消息ID（`UserMessageId`）
- **时间戳**：操作发生的时间（UTC格式）
- **上下文快照**：调用时的项目状态（`ProjectSnapshot`）、Git状态（`GitState`）及工作区快照（`WorktreeSnapshot`）

通过`ThreadsDatabase`接口，用户可查询历史线程的详细信息，包括标题、消息列表、模型配置和累计令牌使用量。此外，`HistoryStore`还维护最近打开的条目列表，便于快速访问和恢复上下文。

**Section sources**
- [db.rs](file://crates/agent2/src/db.rs#L26-L53)
- [thread.rs](file://crates/agent2/src/thread.rs#L1-L799)

## 结论
上下文感知工具API为智能代理系统提供了强大的环境交互与自我解释能力。通过`diagnostics_tool`、`open_tool`、`now_tool`和`thinking_tool`，代理能够获取诊断信息、执行系统操作、访问时间上下文并展示内部推理过程。这些工具与`history_store`和`thread`模块深度集成，确保所有操作均可追溯、可审计，从而构建了一个透明、可靠且可调试的AI代理架构。