# Plan SSE架构统一设计文档

## 背景

在之前的实现中，我们有两个独立的SSE端点：
- `/progress/{session_id}` - 用于一般的任务进度
- `/api/plans/{session_id}/updates` - 用于Plan更新

根据Zed项目的架构分析和用户要求，我们需要将Plan更新合并到统一的progress SSE端点中。

## Zed项目架构分析

通过分析Zed项目，我们发现：

1. **统一的SessionUpdate机制**：Zed使用`acp::SessionUpdate`枚举处理所有类型的更新
   ```rust
   pub enum SessionUpdate {
       UserMessageChunk { content },
       AgentMessageChunk { content },
       AgentThoughtChunk { content },
       ToolCall(tool_call),
       ToolCallUpdate(tool_call_update),
       Plan(plan),  // Plan更新也是SessionUpdate的一部分
       AvailableCommandsUpdate { available_commands },
       CurrentModeUpdate { current_mode_id },
   }
   ```

2. **统一的事件处理**：所有SessionUpdate都通过`handle_session_update`方法处理
   ```rust
   pub fn handle_session_update(&mut self, update: acp::SessionUpdate, cx: &mut Context<Self>) -> Result<(), acp::Error> {
       match update {
           acp::SessionUpdate::Plan(plan) => {
               self.update_plan(plan, cx);
           }
           // 其他更新类型...
       }
   }
   ```

3. **统一的UI事件系统**：所有更新最终通过`AcpThreadEvent`发送给UI层

## 新架构设计

### 1. 统一的ProgressEvent系统

扩展`ProgressEventType`枚举，添加Plan相关事件类型：

```rust
pub enum ProgressEventType {
    // 原有事件类型
    TaskStarted,
    Executing,
    CommandOutput,
    TaskCompleted,
    TaskFailed,
    KeepAlive,
    
    // 新增Plan相关事件类型
    PlanUpdate,        // Plan整体更新
    PlanEntryUpdate,   // Plan条目更新
    PlanStatsUpdate,   // Plan统计更新
}
```

### 2. Plan更新事件转换

创建`plan_update_to_progress_event`函数，将`PlanUpdateEvent`转换为`ProgressEvent`：

```rust
fn plan_update_to_progress_event(plan_update: PlanUpdateEvent) -> ProgressEvent {
    match &plan_update.update_type {
        PlanUpdateType::FullUpdate => { /* 完整Plan更新 */ }
        PlanUpdateType::EntryStatusUpdate { entry_id, status } => { /* 条目状态更新 */ }
        PlanUpdateType::EntryAdded { entry_id } => { /* 新条目添加 */ }
        PlanUpdateType::EntryRemoved { entry_id } => { /* 条目删除 */ }
        PlanUpdateType::StatsUpdate => { /* 统计更新 */ }
    }
}
```

### 3. 统一的progress_stream端点

修改`progress_stream`函数：

1. **订阅Plan更新**：通过`state.plan_manager.subscribe_updates()`订阅Plan更新
2. **后台任务处理**：使用`tokio::spawn`在后台处理Plan更新事件
3. **事件转换和推送**：将Plan更新转换为ProgressEvent并推送给前端

```rust
async fn progress_stream(state, session_id) -> Sse {
    // 1. 创建progress事件通道
    let (tx, rx) = mpsc::unbounded_channel();
    
    // 2. 订阅Plan更新
    let plan_update_rx = state.plan_manager.subscribe_updates().await;
    
    // 3. 后台任务处理Plan更新
    tokio::spawn(async move {
        while let Some(plan_update) = plan_update_rx.recv().await {
            if plan_update.session_id == session_id {
                let progress_event = plan_update_to_progress_event(plan_update);
                tx.send(progress_event);
            }
        }
    });
    
    // 4. 返回统一的SSE流
    Sse::new(stream)
}
```

## API端点变更

### 保留的端点

- `GET /progress/{session_id}` - **统一的SSE端点**，现在处理所有agent数据包括Plan更新
- `GET /api/plans/{session_id}` - Plan查询端点
- `GET /api/plans/stats` - 所有Plan统计信息查询

### 移除的端点

- ~~`GET /api/plans/{session_id}/updates`~~ - Plan专用SSE端点已移除

## 前端集成指南

### 1. SSE连接

前端只需要连接一个SSE端点：

```javascript
const eventSource = new EventSource(`/progress/${sessionId}`);
```

### 2. 事件监听

监听不同类型的事件：

```javascript
// 一般任务事件
eventSource.addEventListener('task_started', (event) => {
    const data = JSON.parse(event.data);
    // 处理任务开始事件
});

// Plan相关事件
eventSource.addEventListener('plan_update', (event) => {
    const data = JSON.parse(event.data);
    if (data.type === 'full_update') {
        // 处理完整Plan更新
        updatePlanDisplay(data.plan);
    }
});

eventSource.addEventListener('plan_entry_update', (event) => {
    const data = JSON.parse(event.data);
    switch (data.type) {
        case 'entry_status_update':
            // 处理条目状态更新
            updateEntryStatus(data.entry_id, data.status);
            break;
        case 'entry_added':
            // 处理新条目添加
            addPlanEntry(data.entry_id);
            break;
        case 'entry_removed':
            // 处理条目删除
            removePlanEntry(data.entry_id);
            break;
    }
});

eventSource.addEventListener('plan_stats_update', (event) => {
    const data = JSON.parse(event.data);
    // 更新Plan统计显示
    updatePlanStats(data.stats);
});
```

### 3. 事件数据结构

所有Plan相关事件都包含在ProgressEvent中：

```typescript
interface ProgressEvent {
    event_type: 'plan_update' | 'plan_entry_update' | 'plan_stats_update' | ...,
    message: string,
    timestamp: string,
    session_id: string,
    data: {
        type: string,
        plan?: Plan,
        entry_id?: string,
        status?: PlanEntryStatus,
        stats?: PlanStats
    }
}
```

## 优势

1. **架构一致性**：与Zed项目的统一SessionUpdate机制保持一致
2. **简化前端集成**：前端只需要连接一个SSE端点
3. **减少连接开销**：避免多个SSE连接的资源消耗
4. **统一的事件处理**：所有agent数据通过相同的机制处理
5. **更好的可维护性**：统一的代码路径，更容易维护和调试

## 测试验证

### 1. Plan更新测试

```bash
# 1. 启动服务器
cargo run

# 2. 连接SSE流
curl -N -H "Accept: text/event-stream" "http://localhost:3000/progress/test-session"

# 3. 触发Plan更新（通过API或agent操作）
# 应该能看到plan_update, plan_entry_update等事件
```

### 2. 兼容性测试

确保原有的progress事件（task_started, executing等）仍然正常工作。

## 迁移指南

对于现有的前端代码：

1. **移除旧的Plan SSE连接**：删除对`/api/plans/{session_id}/updates`的连接
2. **统一事件监听**：将Plan事件监听合并到`/progress/{session_id}`连接中
3. **更新事件处理**：根据新的事件类型和数据结构更新处理逻辑

这个架构变更完全符合用户的要求："Todo list 的plan推送给前端,应该和其他agent 产生的数据一起通过 sse 推送给前端,不应该是单独的一个sse接口推送过去"，并且参考了Zed项目的最佳实践。