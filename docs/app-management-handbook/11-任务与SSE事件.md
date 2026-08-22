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

### POST `/apps/publish/tasks/query` — 任务列表分页查询

枚举任务（免调用方自记 task_id）。POST body 承载过滤与分页（与 `/apps/query` 惯例一致）。

**请求体**：
```jsonc
{
  "page": 1,
  "page_size": 20,
  "filters": {
    "app_ids": ["app-order-svc"],   // 可选：按 app 过滤
    "kind": "build",               // 可选：build（publish 已废弃，仅为读存量历史行保留）
    "active_only": true             // 可选：只看未终态（对账：该 app 在跑任务吗）
  }
}
```

- `page` 默认 1（<1 → 400）；`page_size` 默认 20，范围 1..=100（越界 → 400）；`filters` 可省略。
- 排序 `createdAt DESC, taskId DESC`。

**响应** `HttpResult<PaginatedResponse<PublishTaskSnapshot>>`：
```jsonc
{
  "success": true,
  "data": {
    "items": [ { "id": "019...", "app_id": "app-order-svc", "kind": "build", "status": "running", "stage": "Build", "seq": 15, ... } ],
    "pagination": { "page": 1, "page_size": 20, "total": 1, "total_pages": 1 }
  }
}
```

> **两种部署模式的语义差异**：
> - **K8s + PG（多副本）**：查 PG 行——覆盖多副本、rcoder 重启、内存容量驱逐；窗口=终态 24h TTL（与单查口径一致）。`stage` 为异步落库快照（秒级滞后），实时进度请走单查/SSE。
> - **Docker Compose（无 PG，单副本）**：遍历 rcoder 进程内存任务表——单副本即全量，但 **rcoder 重启后列表为空**（活任务随进程消亡，PG 回退也不存在）。

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
      "kind": "build",              // build（publish 已废弃）
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
data: {"event":"<事件类型>", <事件字段...>}   // data 为事件对象全量（内嵌 event 标签）

```

**事件类型清单**（rcoder 任务 SSE 实际事件；字段 snake_case）：
| event | 说明 | data 关键字段 |
|---|---|---|
| `stage` | 阶段变更 | `stage`：rcoder 侧 `EnsureBuilder`/`Build`/`Prepare`/`Activate` |
| `build_progress` | agent-runner 构建进度原样透传 | `data`：agent-runner 事件对象（**内部字段保持 camelCase**，如 `releaseId`/`sizeBytes`/`buildOk`——与 agent-runner 链路一致；终态判定用外层事件，无需解析内层） |
| `cancelling` | 取消已请求（非终态，通知前端"取消中"） | — |
| `completed` | 终态（成功） | `release_id` |
| `failed` | 终态（失败） | `error` |
| `cancelled` | 终态（取消） | — |

> **Builder 自动创建**：`build` 触发时若 UserAppBuilder 容器不存在（含 rcoder 重启后注册丢失），任务会先经 `stage=EnsureBuilder` 自动创建并注册（K8s 拉镜像可能数十秒），再进入 `stage=Build`。创建失败以任务 `failed` 终态呈现。

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
