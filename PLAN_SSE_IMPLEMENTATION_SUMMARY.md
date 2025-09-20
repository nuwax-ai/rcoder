# Plan SSE架构统一 - 实现总结

## 完成的工作

根据您的要求："Todo list 的plan推送给前端,应该和其他agent 产生的数据一起通过 sse 推送给前端,不应该是单独的一个sse接口推送过去"，我们成功实现了Plan SSE架构的统一。

### 1. 架构分析

通过深入分析Zed项目源码，我们发现：
- Zed使用统一的`SessionUpdate`机制处理所有更新类型
- 所有更新都通过`handle_session_update`方法统一处理
- Plan更新是`SessionUpdate::Plan(plan)`的一部分
- 所有事件最终通过`AcpThreadEvent`发送给UI层

### 2. 代码修改

#### 扩展ProgressEventType
```rust
pub enum ProgressEventType {
    // 原有事件类型
    TaskStarted, Executing, CommandOutput, TaskCompleted, TaskFailed, KeepAlive,
    
    // 新增Plan相关事件类型
    PlanUpdate,        // Plan整体更新
    PlanEntryUpdate,   // Plan条目更新  
    PlanStatsUpdate,   // Plan统计更新
}
```

#### 统一的progress_stream端点
- 订阅Plan更新：`state.plan_manager.subscribe_updates()`
- 后台任务处理：使用`tokio::spawn`处理Plan更新事件
- 事件转换：通过`plan_update_to_progress_event`转换为统一格式
- 统一推送：所有事件通过同一个SSE流推送

#### 移除独立的Plan SSE端点
- 移除了`/api/plans/{session_id}/updates`端点
- 保留Plan查询端点`/api/plans/{session_id}`和`/api/plans/stats`

### 3. 架构优势

✅ **统一性**：与Zed项目的SessionUpdate机制保持一致  
✅ **简化性**：前端只需连接一个SSE端点  
✅ **高效性**：减少连接开销，统一事件处理  
✅ **可维护性**：统一的代码路径，更容易维护和调试  

### 4. API变更

| 端点 | 状态 | 说明 |
|------|------|------|
| `GET /progress/{session_id}` | ✅ 保留并增强 | 统一SSE端点，现在处理所有agent数据包括Plan更新 |
| `GET /api/plans/{session_id}` | ✅ 保留 | Plan查询端点 |
| `GET /api/plans/stats` | ✅ 保留 | Plan统计查询 |
| ~~`GET /api/plans/{session_id}/updates`~~ | ❌ 已移除 | Plan专用SSE端点已移除 |

### 5. 前端集成

前端现在只需要连接一个SSE端点：

```javascript
const eventSource = new EventSource(`/progress/${sessionId}`);

// 监听Plan相关事件
eventSource.addEventListener('plan_update', handlePlanUpdate);
eventSource.addEventListener('plan_entry_update', handlePlanEntryUpdate);  
eventSource.addEventListener('plan_stats_update', handlePlanStatsUpdate);

// 同时还能监听其他agent事件
eventSource.addEventListener('task_started', handleTaskStarted);
eventSource.addEventListener('executing', handleExecuting);
```

### 6. 测试验证

✅ 统一SSE端点正常工作  
✅ Plan查询API正常工作  
✅ Plan统计API正常工作  
✅ 旧的Plan专用SSE端点已正确移除  

### 7. 文档和示例

- 📖 详细架构文档：`PLAN_SSE_UNIFICATION.md`
- 🧪 测试脚本：`test_unified_plan_sse.sh`
- 💻 前端集成示例：`plan_sse_frontend_example.js`

## 结果

现在Plan推送完全集成到统一的`/progress/{session_id}`SSE流中，与其他agent数据一起推送给前端，完全符合您的架构要求。这个实现参考了Zed项目的最佳实践，确保了架构的一致性和可维护性。