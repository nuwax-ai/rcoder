// Plan SSE统一架构 - 前端集成示例

class UnifiedPlanSSE {
    constructor(sessionId) {
        this.sessionId = sessionId;
        this.eventSource = null;
    }

    connect() {
        // 只需要连接一个统一的SSE端点
        this.eventSource = new EventSource(`/progress/${this.sessionId}`);
        
        // 监听连接事件
        this.eventSource.addEventListener('keep_alive', (event) => {
            console.log('SSE连接保持活跃');
        });

        // 监听一般任务事件
        this.eventSource.addEventListener('task_started', (event) => {
            const data = JSON.parse(event.data);
            console.log('任务开始:', data);
        });

        this.eventSource.addEventListener('executing', (event) => {
            const data = JSON.parse(event.data);
            console.log('任务执行中:', data);
        });

        this.eventSource.addEventListener('task_completed', (event) => {
            const data = JSON.parse(event.data);
            console.log('任务完成:', data);
        });

        // 监听Plan相关事件 (新增)
        this.eventSource.addEventListener('plan_update', (event) => {
            const data = JSON.parse(event.data);
            console.log('Plan更新:', data);
            
            if (data.data.type === 'full_update') {
                this.updatePlanDisplay(data.data.plan);
            }
        });

        this.eventSource.addEventListener('plan_entry_update', (event) => {
            const data = JSON.parse(event.data);
            console.log('Plan条目更新:', data);
            
            switch (data.data.type) {
                case 'entry_status_update':
                    this.updateEntryStatus(data.data.entry_id, data.data.status);
                    break;
                case 'entry_added':
                    this.addPlanEntry(data.data.entry_id);
                    break;
                case 'entry_removed':
                    this.removePlanEntry(data.data.entry_id);
                    break;
            }
        });

        this.eventSource.addEventListener('plan_stats_update', (event) => {
            const data = JSON.parse(event.data);
            console.log('Plan统计更新:', data);
            this.updatePlanStats(data.data.stats);
        });

        // 错误处理
        this.eventSource.onerror = (error) => {
            console.error('SSE连接错误:', error);
        };
    }

    disconnect() {
        if (this.eventSource) {
            this.eventSource.close();
            this.eventSource = null;
        }
    }

    // Plan显示更新方法
    updatePlanDisplay(plan) {
        console.log('更新Plan显示:', plan);
        // 实现Plan UI更新逻辑
    }

    updateEntryStatus(entryId, status) {
        console.log(`更新条目${entryId}状态为:`, status);
        // 实现条目状态更新逻辑
    }

    addPlanEntry(entryId) {
        console.log('添加新Plan条目:', entryId);
        // 实现新条目添加逻辑
    }

    removePlanEntry(entryId) {
        console.log('移除Plan条目:', entryId);
        // 实现条目移除逻辑
    }

    updatePlanStats(stats) {
        console.log('更新Plan统计:', stats);
        // 实现统计信息更新逻辑
    }
}

// 使用示例
// const planSSE = new UnifiedPlanSSE('session-123');
// planSSE.connect();
