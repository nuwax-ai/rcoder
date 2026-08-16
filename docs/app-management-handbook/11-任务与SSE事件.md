# 11. 构建发布任务与 SSE 事件

> 构建发布是异步任务，本文说明任务状态查询与 SSE 实时进度事件格式。

---

## 任务生命周期

```
Pending → Running → (Cancelling) → Completed / Failed / Cancelled
                ↑                    ↑
           request_cancel       终态后事件全部丢弃
```

**终态后**：
- 后续 emit 事件全部丢弃
- 任务在内存中保留 24h（供前端断线重连查询）
- PG 中保留 24h 后自动 TTL 清理

---

## 任务查询接口

### GET `/apps/publish/tasks/{task_id}` — 任务状态快照

**响应** `HttpResult<GetTaskData>`：
```jsonc
{
  "success": true,
  "data": {
    "task": {
      "id": "019123456789abcdef",
      "app_id": "app-order-svc",
      "project_id": "app-order-svc",
      "kind": "build",              // build | publish
      "status": "running",          // pending | running | cancelling | completed | failed | cancelled
      "stage": "compiling",         // 当前阶段标识
      "release_id": null,           // completed 时回填
      "error": null,                // failed 时的错误文案
      "seq": 15,                    // 当前事件序号（断线重连游标）
      "created_at": 1786738599,
      "updated_at": 1786738605
    }
  }
}
```

**错误**：
| HTTP | code | 说明 |
|---|---|---|
| 404 | ERR_NOT_FOUND | 任务不存在 |

> **跨重启可见**：rcoder 重启后，PG 中保留的任务行仍可查询（状态回退为 `failed("rcoder restarted")`）。

---

### GET `/apps/publish/tasks/{task_id}/stream` — SSE 实时进度

**请求参数**：
| 参数 | 类型 | 说明 |
|---|---|---|
| `from_seq` | u64（query） | 断线重游标（上次收到的最大 seq；0=从头回放） |

Last-Event-ID header 优先于 query 参数。

**SSE 事件格式**：
```
event: <事件类型>
id: <seq>
data: {"taskId":"...","kind":"build","status":"running",...}

```

**事件类型清单**：
| event | 说明 | data 关键字段 |
|---|---|---|
| `task_created` | 任务创建 | taskId, kind |
| `stage` | 阶段变更 | stage: "downloading"/"compiling"/"packaging" |
| `build_progress` | 构建进度 chunk | data: {content: "..."} |
| `task_completed` | 终态（成功） | releaseId |
| `task_failed` | 终态（失败） | error |
| `task_cancelled` | 终态（取消） | — |
| `stream_lagged` | 消费者太慢被断开 | — （客户端应用 from_seq 重连） |

**消费端去重**：按 `id`（seq）去重——`seq <= 已收最大值` 的事件丢弃。

**断线重连**：
```bash
# 记住 last_seq，重连时传 from_seq
LAST_SEQ=15
curl -N "$RCODER/api/v1/apps/publish/tasks/$TASK_ID/stream?from_seq=$LAST_SEQ"
```

---

### POST `/apps/publish/tasks/{task_id}/cancel` — 取消任务

**请求体**：无

**响应** `HttpResult<CancelTaskData>`：
```jsonc
{
  "success": true,
  "data": {
    "accepted": true,        // true=已进入 Cancelling
    "status": "cancelling"   // AlreadyTerminal 时返回实际终态
  }
}
```

> 取消是**请求**而非同步完成：远端 build 取消/回滚由 orchestrator 异步收敛。

---

### POST `/apps/{app_id}/ensure-builder` — 确保 Builder 容器存在

手动触发创建 UserApp Builder 容器（通常 build/publish 自动调用，一般无需手动）。

**响应** `HttpResult<EnsureBuilderData>`：
```jsonc
{
  "success": true,
  "data": {
    "app_id": "app-order-svc",
    "container_name": "rcoder-app-app-order-svc-builder",
    "container_ip": "10.42.0.123"
  }
}
```

---

## 并发约束（U2：同 app 单活跃任务）

同一 app 同时只允许一个活跃（未终态）的 build/publish 任务。

- 进程内：`PublishTaskStore` 的 Mutex 扫描 → 409 `ERR_CONFLICT`
- 跨进程/跨副本：PG 部分唯一索引 `UNIQUE(app_id) WHERE terminal_at IS NULL` → 409

**重试时机**：前一个任务进入终态（completed/failed/cancelled）后即可再建新任务。

---

## curl 完整示例：监听构建进度

```bash
TASK_ID="019123456789abcdef"
LAST_SEQ=0

while true; do
  # SSE 流式消费，每条事件带 id: <seq>
  curl -sN "$RCODER/api/v1/apps/publish/tasks/$TASK_ID/stream?from_seq=$LAST_SEQ" | \
  while IFS= read -r line; do
    case "$line" in
      id:*) LAST_SEQ="${line#id: }" ;;
      event:task_completed|event:task_failed|event:task_cancelled)
        echo "Terminal: $line"; exit 0 ;;
      *) echo "$line" ;;
    esac
  done
  [ $? -eq 0 ] && break
  sleep 2  # Lagged/断开后短暂重试
done
```

---

## 下一步

- 完整字段定义看 Swagger：`http://$RCODER/api/docs` → tag「UserApp 发布」
