# 目录管理API

<cite>
**本文档中引用的文件**  
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs)
- [worktree_store.rs](file://crates/project/src/worktree_store.rs)
</cite>

## 目录

1. [简介](#简介)
2. [项目结构](#项目结构)
3. [核心组件](#核心组件)
4. [架构概览](#架构概览)
5. [详细组件分析](#详细组件分析)
6. [依赖分析](#依赖分析)
7. [性能考虑](#性能考虑)
8. [故障排除指南](#故障排除指南)
9. [结论](#结论)

## 简介
本文档系统化地记录了目录管理相关的API端点，包括创建、删除和查询目录的操作。重点涵盖 `POST /projects/:project_id/directory`、`DELETE /projects/:project_id/path` 和 `GET /projects/:project_id/directory/:path` 三个核心端点。文档详细说明了各接口的请求体格式、状态码语义以及递归操作的限制条件。同时，阐述了 `create_directory_tool` 和 `delete_path_tool` 如何与 `worktree_store` 交互以维护项目树结构的一致性，并说明 `list_directory_tool` 在AI代理决策链中的前置探查作用。最后，提供了防止路径遍历攻击的安全实践指南。

## 项目结构
目录管理功能分布在多个模块中，主要涉及工具层（tools）和项目存储层（project）。工具层负责提供对外的API接口，而项目存储层则负责底层的文件系统操作和状态维护。

```mermaid
graph TD
subgraph "工具层"
CDTool["create_directory_tool"]
DPTool["delete_path_tool"]
LDTool["list_directory_tool"]
end
subgraph "项目层"
WorktreeStore["worktree_store"]
Project["Project"]
end
CDTool --> WorktreeStore
DPTool --> WorktreeStore
LDTool --> WorktreeStore
WorktreeStore --> Project
```

**Diagram sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)
- [worktree_store.rs](file://crates/project/src/worktree_store.rs#L1-L1004)

**Section sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)
- [worktree_store.rs](file://crates/project/src/worktree_store.rs#L1-L1004)

## 核心组件
本节分析三个核心工具组件：`create_directory_tool`、`delete_path_tool` 和 `list_directory_tool`，它们分别对应目录的创建、删除和查询操作。

**Section sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)

## 架构概览
目录管理API的整体架构分为工具层和存储层。工具层接收外部请求并进行参数验证，然后调用项目层的相应方法。项目层通过 `worktree_store` 管理工作树的状态，并与底层文件系统交互。

```mermaid
sequenceDiagram
participant Client as "客户端"
participant Tool as "工具层"
participant Project as "项目层"
participant Worktree as "WorktreeStore"
participant FS as "文件系统"
Client->>Tool : 发送API请求
Tool->>Project : 调用项目方法
Project->>Worktree : 更新工作树状态
Worktree->>FS : 执行文件系统操作
FS-->>Worktree : 返回操作结果
Worktree-->>Project : 返回状态更新
Project-->>Tool : 返回执行结果
Tool-->>Client : 返回响应
```

**Diagram sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)
- [worktree_store.rs](file://crates/project/src/worktree_store.rs#L1-L1004)

## 详细组件分析
本节详细分析每个组件的实现细节、交互逻辑和安全机制。

### 创建目录工具分析
`create_directory_tool` 负责创建新的目录，支持递归创建父目录。

#### 类图
```mermaid
classDiagram
class CreateDirectoryTool {
+project : Entity~Project~
+new(project : Entity~Project~) CreateDirectoryTool
+name() string
+kind() ToolKind
+initial_title(input : Result~Input, Value~, cx : &mut App) SharedString
+run(self : Arc~Self~, input : Input, event_stream : ToolCallEventStream, cx : &mut App) Task~Result~Output~~
}
class CreateDirectoryToolInput {
+path : String
}
CreateDirectoryTool ..|> AgentTool : 实现
AgentTool <|-- CreateDirectoryTool
```

**Diagram sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)

#### 执行流程
```mermaid
flowchart TD
Start([开始]) --> ValidatePath["验证路径有效性"]
ValidatePath --> PathValid{"路径有效?"}
PathValid --> |否| ReturnError["返回路径错误"]
PathValid --> |是| FindProjectPath["查找项目路径"]
FindProjectPath --> CreateEntry["创建条目"]
CreateEntry --> AwaitCreate["等待创建完成"]
AwaitCreate --> CheckResult{"创建成功?"}
CheckResult --> |否| HandleCreateError["处理创建错误"]
CheckResult --> |是| ReturnSuccess["返回成功消息"]
HandleCreateError --> ReturnError
ReturnSuccess --> End([结束])
ReturnError --> End
```

**Diagram sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)

**Section sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)

### 删除路径工具分析
`delete_path_tool` 负责删除文件或目录，支持递归删除目录内容。

#### 类图
```mermaid
classDiagram
class DeletePathTool {
+project : Entity~Project~
+action_log : Entity~ActionLog~
+new(project : Entity~Project~, action_log : Entity~ActionLog~) DeletePathTool
+name() string
+kind() ToolKind
+initial_title(input : Result~Input, Value~, cx : &mut App) SharedString
+run(self : Arc~Self~, input : Input, event_stream : ToolCallEventStream, cx : &mut App) Task~Result~Output~~
}
class DeletePathToolInput {
+path : String
}
DeletePathTool ..|> AgentTool : 实现
AgentTool <|-- DeletePathTool
```

**Diagram sources**
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)

#### 执行流程
```mermaid
flowchart TD
Start([开始]) --> ValidatePath["验证路径有效性"]
ValidatePath --> PathValid{"路径有效?"}
PathValid --> |否| ReturnError["返回路径错误"]
PathValid --> |是| FindProjectPath["查找项目路径"]
FindProjectPath --> FindWorktree["查找工作树"]
FindWorktree --> Snapshot["获取工作树快照"]
Snapshot --> Traverse["遍历路径内容"]
Traverse --> BufferCheck["检查缓冲区"]
BufferCheck --> ActionLog["记录删除操作"]
ActionLog --> DeleteFile["删除文件"]
DeleteFile --> AwaitDelete["等待删除完成"]
AwaitDelete --> CheckResult{"删除成功?"}
CheckResult --> |否| HandleDeleteError["处理删除错误"]
CheckResult --> |是| ReturnSuccess["返回成功消息"]
HandleDeleteError --> ReturnError
ReturnSuccess --> End([结束])
ReturnError --> End
```

**Diagram sources**
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)

**Section sources**
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)

### 列出目录工具分析
`list_directory_tool` 负责列出指定路径下的文件和目录内容。

#### 类图
```mermaid
classDiagram
class ListDirectoryTool {
+project : Entity~Project~
+new(project : Entity~Project~) ListDirectoryTool
+name() string
+kind() ToolKind
+initial_title(input : Result~Input, Value~, cx : &mut App) SharedString
+run(self : Arc~Self~, input : Input, event_stream : ToolCallEventStream, cx : &mut App) Task~Result~Output~~
}
class ListDirectoryToolInput {
+path : String
}
ListDirectoryTool ..|> AgentTool : 实现
AgentTool <|-- ListDirectoryTool
```

**Diagram sources**
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)

#### 执行流程
```mermaid
flowchart TD
Start([开始]) --> HandleSpecialCase["处理特殊路径"]
HandleSpecialCase --> SpecialPath{"路径为 ., ./, *, 或空?"}
SpecialPath --> |是| ListRoot["列出根目录"]
SpecialPath --> |否| ValidatePath["验证路径"]
ValidatePath --> FindProjectPath["查找项目路径"]
FindProjectPath --> FindWorktree["查找工作树"]
FindWorktree --> CheckExclusion["检查排除设置"]
CheckExclusion --> Excluded{"路径被排除?"}
Excluded --> |是| ReturnExclusionError["返回排除错误"]
Excluded --> |否| GetEntry["获取条目"]
GetEntry --> IsDirectory{"是目录?"}
IsDirectory --> |否| ReturnNotDirError["返回非目录错误"]
IsDirectory --> |是| GetChildren["获取子条目"]
GetChildren --> FilterEntries["过滤条目"]
FilterEntries --> Categorize["分类文件和文件夹"]
Categorize --> FormatOutput["格式化输出"]
FormatOutput --> ReturnResult["返回结果"]
ListRoot --> ReturnResult
ReturnExclusionError --> End([结束])
ReturnNotDirError --> End
ReturnResult --> End
```

**Diagram sources**
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)

**Section sources**
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)

## 依赖分析
目录管理工具依赖于项目层的 `worktree_store` 来维护工作树状态和执行文件系统操作。

```mermaid
erDiagram
CREATE_DIRECTORY_TOOL {
string path PK
}
DELETE_PATH_TOOL {
string path PK
}
LIST_DIRECTORY_TOOL {
string path PK
}
WORKTREE_STORE {
int worktree_id PK
string root_name
bool visible
}
PROJECT {
int project_id PK
}
CREATE_DIRECTORY_TOOL ||--o{ WORKTREE_STORE : "使用"
DELETE_PATH_TOOL ||--o{ WORKTREE_STORE : "使用"
LIST_DIRECTORY_TOOL ||--o{ WORKTREE_STORE : "使用"
WORKTREE_STORE ||--o{ PROJECT : "属于"
```

**Diagram sources**
- [worktree_store.rs](file://crates/project/src/worktree_store.rs#L1-L1004)

**Section sources**
- [worktree_store.rs](file://crates/project/src/worktree_store.rs#L1-L1004)

## 性能考虑
- **并发操作**：所有工具操作都在后台任务中执行，避免阻塞主线程。
- **批量处理**：删除操作会批量处理目录内容，减少I/O开销。
- **缓存机制**：`worktree_store` 维护工作树快照，减少重复的文件系统访问。
- **流式处理**：大目录的遍历采用流式处理，避免内存溢出。

## 故障排除指南
### 常见问题及解决方案
- **路径不存在**：确保提供的路径在项目范围内，且父目录已存在（创建目录时除外）。
- **权限不足**：检查文件系统权限，确保应用有读写权限。
- **路径被排除**：检查 `file_scan_exclusions` 和 `private_files` 设置，确保路径未被排除。
- **递归删除失败**：对于大型目录，可能需要更长的超时时间或分批处理。

**Section sources**
- [create_directory_tool.rs](file://crates/agent2/src/tools/create_directory_tool.rs#L1-L90)
- [delete_path_tool.rs](file://crates/agent2/src/tools/delete_path_tool.rs#L1-L140)
- [list_directory_tool.rs](file://crates/agent2/src/tools/list_directory_tool.rs#L1-L670)

## 结论
本文档全面记录了目录管理API的设计、实现和使用方法。通过 `create_directory_tool`、`delete_path_tool` 和 `list_directory_tool` 三个核心组件，系统提供了完整的目录管理功能。这些工具通过 `worktree_store` 与项目层紧密集成，确保了项目树结构的一致性和安全性。同时，系统实现了完善的安全机制，防止路径遍历攻击和敏感文件访问。建议在使用这些API时，遵循最佳实践，合理处理错误情况，并注意性能优化。