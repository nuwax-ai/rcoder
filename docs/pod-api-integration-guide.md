# RCoder Pod 观测接口 —— 对接文档

> 目标：用 3 个接口复刻 OOM 观测面板（容器实时资源 + 命名空间概览 + 健康/重启归因）。

---

## 0. 环境信息

| 项 | 值 |
|---|---|
| **在线文档（Scalar，可交互调试）** | `http://<test-cluster-host>:<rcoder-nodeport>/api/docs/scalar` |
| OpenAPI JSON | `http://<test-cluster-host>:<rcoder-nodeport>/api/docs/openapi.json` |
| 服务地址 | `http://<test-cluster-host>:<rcoder-nodeport>` |
| 命名空间 | `nuwax-k8s-test` |
| 当前版本 | `rcoder-k8s:0.1.452` |
| 测试用 user_id | `6`（已有 `rcoder-computer-agent-runner-6-0` 在跑） |

> ⚠️ 上面是**内网测试环境**，会随时重新部署。字段以本文档 + 在线 Scalar 页为准。

### 参数同源约定

三个接口用**同一套定位参数**（与 `/computer/pod/ensure` 一致）：

| 参数 | 说明 |
|---|---|
| `user_id` | computer 族容器标识（最常用） |
| `project_id` | web 族标识 |
| `pod_id` | 直接指定（多租户） |
| `service_type` | 缺省 `computer-agent-runner`；可选 `WebAgentRunner` / `ComputerNormalProject` / `Userapp` / `UserappBuilder` |
| `app_id` + `app_stage` | userapp 族（`dev`→构建容器，`prod`→业务容器） |

**只传 `user_id` 即可覆盖绝大多数场景。**

---

## 1. 三个接口速览

| 接口 | 方式 | 用途 | 建议频率 |
|---|---|---|---|
| `/computer/pod/resources/stream` | **SSE** | 单容器**实时**资源（CPU/内存/进程/会话） | 长连接，服务端推送 |
| `/computer/pod/overview` | GET | **全部 Pod** 的规格/健康/用量/事件 | 5~10s 或打开页面时一次 |
| `/computer/pod/health` | GET | 单容器**健康 + 最后一次重启原因** | 3~5s |

---

## 2. 接口详解

### 2.1 `GET /computer/pod/resources/stream` —— 实时资源（SSE）

**用途**：主面板的 CPU/内存曲线、进程数、Agent 会话列表。

```
GET /computer/pod/resources/stream?user_id=6&interval_secs=2
Accept: text/event-stream
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `interval_secs` | 2 | 推送间隔，clamp 1~30 |
| `include_processes` | true | **必须为 true 才有进程数据** |
| `user_id` / `project_id` / `pod_id` | — | 定位容器（同上） |

**推送格式**（`event: resources`）：

```
event: resources
data: {"ts":"2026-11-18T12:43:45.082621157+00:00","container":{"alive":true,"cpu_percent":0.7945,"memory_mb":599,"memory_limit_mb":6144,"cpu_limit_cores":2.0},"agent":{"alive":false,"status":"Idle","cpu_percent":null,"memory_mb":null},"processes":[{"pid":380,"comm":"chrome-devtools","rss_mb":149.22,"cpu_seconds":0.73,"group":"platform"},...],"agents":[],"process_groups":{"infra_rss_mb":1065.76,"platform_rss_mb":400.75,"workload_rss_mb":0.0}}
```

字段说明：

| 字段 | 含义 |
|---|---|
| `ts` | 采集时刻 |
| `container.alive` | 容器是否存活 |
| `container.cpu_percent` | **占 limit 的百分比**（无 limit 时口径见文档） |
| `container.memory_mb` / `memory_limit_mb` | 容器 cgroup 实时内存 / 限额 |
| `container.cpu_limit_cores` | CPU 限额（核） |
| `agent.alive` | 容器内 agent 进程是否存活（`null` = 未知，**不要当 false**） |
| `agent.status` | `Idle` / `Running` 等 |
| `processes[]` | 进程明细：`pid` / `comm` / `rss_mb` / `cpu_seconds` / `group` |
| `processes[].group` | `infra`（基础设施）/ `platform`（平台）/ `workload`（业务） |
| `agents[]` | **每个 project 一条**的会话聚合：`project_id` / `session_id` / `status` / `rss_mb` / `cpu_seconds` / `process_count` |
| `process_groups` | 三组 RSS 汇总（MB） |

**注意**：
- `processes` / `agents` / `process_groups` **缺省 = 本轮未采样**，与 `[]`（采样了但确实 0 条）语义不同；
- **容器死亡时流会断开**（数据源在容器内）。感知 OOM/重启请用 `/computer/pod/health`；
- 连续失败约 30s 服务端会关流，客户端需自行重连。

**如果不需要实时推送**，用 `GET /computer/pod/resources`（同参数、同数据、普通 GET），省掉重连逻辑。

### 2.2 `GET /computer/pod/overview` —— 命名空间概览

**用途**：概览表格（全部 Pod 一行一条）+ 顶部汇总。

```
GET /computer/pod/overview?user_id=6&events_limit=5
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `search` | — | 关键字过滤（Pod 名 / user_id / app_id / project_id） |
| `events_limit` | 20 | 每个 Pod 返回的最近事件数（1~50） |
| `include_sidecars` | false | 是否带内部 sidecar（log-shipper 等） |
| `limit` | 500 | 返回条数上限 |

**响应**（真实数据，已删减）：

```jsonc
{
  "code": "0000",
  "data": {
    "pods": [
      {
        "name": "rcoder-computer-agent-runner-6-0",          // Pod 名
        "workload": { "kind": "StatefulSet", "name": "rcoder-computer-agent-runner-6" },
        "identity": { "service_type": "computer-agent-runner", "user_id": "6" },
        "pod_uid": "63b613cd-d220-43f2-b748-097ad73650c6",
        "phase": "Running",
        "ready": true,
        "node": "soddy",
        "ip": "10.42.1.22",
        "created_at": "2026-11-18T12:29:00Z",
        "spec": {
          "cpu_request_cores": 0.05, "cpu_limit_cores": 2.0,
          "mem_request_mb": 1024, "mem_limit_mb": 6144,
          "ephemeral_storage_limit_mb": 51200,
          "volumes": [ { "type": "pvc", "name": "workspace", "mount_path": "/home/user", "capacity_mb": 62914560 } ],
          "containers": [ { "name": "agent", "cpu_limit_cores": 2.0, "mem_limit_mb": 6144 } ]
        },
        "health": {
          "restart_count": 0,
          "restart_source": "container_status",
          "last_termination": { "reason": "OOMKilled", "exit_code": 137,
                                "finished_at": "2026-11-18T04:07:26Z", "is_oom": true },
          "waiting_reason": null
        },
        "usage": {
          "source": "metrics_server", "window": "15.07s",
          "cpu_cores": 0.0123, "cpu_percent": 0.62, "cpu_percent_basis": "limit",
          "mem_used_mb": 608, "mem_limit_mb": 6144,
          "process_count": null
        },
        "events": [ { "event_type": "Warning", "reason": "Unhealthy", "message": "...", "timestamp": "...", "count": 11 } ]
      }
    ],
    "summary": { "pods": 84, "running": 70, "not_ready": 1, "restarted": 24,
                 "oom_killed_last": 0, "with_warning_events": 19 },
    "degraded": [],
    "source": "kubernetes",
    "namespace": "nuwax-k8s-test",
    "cache_age_seconds": 3
  }
}
```

**关键语义**：
- `degraded: []` = 所有分区采集成功。**空数组 ≠ 没数据**，用 `pods.length` 判断；
- `usage` 缺省 = 采集不到（metrics-server 未装 / 容器未运行），**不是 0**；
- 排序：异常优先（未就绪 > OOM > 有重启），已完成的 Job 沉底；
- 有 **10 秒缓存**。

### 2.3 `GET /computer/pod/health` —— 健康 + 最后一次重启原因

**用途**：单容器健康卡片、OOM/重启告警。**这是查"最后一次重启原因"的权威接口。**

```
GET /computer/pod/health?user_id=6
GET /computer/pod/health?user_id=6&fresh=true     // 绕过 3s 缓存
```

**响应**（真实数据）：

```json
{
  "code": "0000",
  "data": {
    "health": {
      "container_name": "rcoder-computer-agent-runner-6",
      "pod_name": "rcoder-computer-agent-runner-6-0",
      "pod_uid": "63b613cd-d220-43f2-b748-097ad73650c6",
      "alive": true,
      "phase": "Running",
      "ready": true,
      "restart_count": 0,
      "started_at": "2026-11-18T12:29:00Z",
      "last_termination": {
        "reason": "OOMKilled",
        "exit_code": 137,
        "finished_at": "2026-11-18T04:07:26Z",
        "is_oom": true
      },
      "sampled_at": "2026-11-18T12:43:45Z",
      "source": "kubernetes"
    },
    "cache_age_seconds": 0
  }
}
```

**`last_termination` 是"最后一次重启原因"，规则如下：**

| 字段 | 说明 |
|---|---|
| `reason` | `OOMKilled` / `Error` / `Completed` / `Unknown` |
| `exit_code` | 退出码 |
| `finished_at` | 终止时刻 |
| `is_oom` | 是否 OOM（`reason == "OOMKilled"` 的派生位） |

- **字段缺省 = 从未终止过**（不是空对象）；
- **判断 OOM 只用 `is_oom`**；
- ❌ **绝不能用 `exit_code == 137` 推断 OOM** —— 探针 SIGKILL 也是 137，但 `is_oom=false`（真实场景实测过）；
- 只能拿到**最近一次**，历史次数拿不到（k8s 只保留 `lastState` 一个槽位）。

---

## 3. Demo 界面复刻指南

### 主面板卡片（实时）

| UI 元素 | 字段 |
|---|---|
| CPU 百分比 | `stream.container.cpu_percent` |
| CPU 核数（分母） | `stream.container.cpu_limit_cores` |
| 内存 `613MB / 6144MB` | `stream.container.memory_mb` / `memory_limit_mb` |
| 距限额 `90% 余量` | `1 - memory_mb / memory_limit_mb` |
| 进程数 `72` | `stream.processes.length` |
| 曲线 | 前端累积每帧的 `cpu_percent`/`memory_mb` |

### 命名空间概览表

| 列 | 字段 |
|---|---|
| Pod | `overview.pods[].name` |
| 身份 | `pods[].identity`（`service_type` + `user_id`/`app_id`/`project_id`） |
| Phase / Ready | `pods[].phase` / `ready` |
| 内存限额 / 使用 | `pods[].spec.mem_limit_mb` / `pods[].usage.mem_used_mb` |
| CPU 限额 / 使用 | `pods[].spec.cpu_limit_cores` / `pods[].usage.cpu_cores` |
| 重启次数 | `pods[].health.restart_count` |
| 上次退出 | `pods[].health.last_termination` |
| 等待原因 | `pods[].health.waiting_reason` |
| 事件数 | `pods[].events` 中 `event_type === "Warning"` 的条数 |
| 顶部汇总 | `overview.summary` |

### 健康卡片（单容器）

| 项 | 字段 |
|---|---|
| 重启次数 | `health.restart_count` |
| 上次退出 + 时间 | `health.last_termination` |
| 等待原因 | `health.waiting_reason` |
| Phase / Ready | `health.phase` / `ready` |
| 限额 | 用 `overview.pods[].spec`（health 里没有 spec） |

---

## 4. 三个必须知道的约定

### 4.1 容器标识：`container_name` vs `pod_name` vs `pod_uid`

| 字段 | 示例 | 跨重建 | 用途 |
|---|---|---|---|
| `container_name` | `...-runner-6` | **不变** | **业务关联键** |
| `pod_name` | `...-runner-6-0` | 可能变 | 排障、`kubectl logs` |
| `pod_uid` | `63b613cd-...` | **必变** | 判断"换了一代容器" |

对应关系（已实测）：

```
health.container_name  ==  overview.workload.name
health.pod_name        ==  overview.pods[].name
health.pod_uid         ==  overview.pods[].pod_uid
```

### 4.2 两个内存数值不一样，不是 bug

```
stream.container.memory_mb  = 599MB   ← 容器 cgroup 实时值
overview.usage.mem_used_mb  = 608MB   ← metrics-server 窗口平均（~15s）
```

**口径不同**。挑一个用，别混着算。

### 4.3 时间格式

全部 **RFC3339、秒精度**：`2026-11-18T04:07:26Z`。唯一例外是 SSE 的 `ts`（纳秒），前端做展示时可截断。

---

## 5. 代码示例

### 5.1 订阅实时资源（SSE）

```js
const es = new EventSource(
  `${BASE}/computer/pod/resources/stream?user_id=6&interval_secs=2`
);

es.addEventListener('resources', (e) => {
  const frame = JSON.parse(e.data);

  renderCpu(frame.container.cpu_percent, frame.container.cpu_limit_cores);
  renderMem(frame.container.memory_mb, frame.container.memory_limit_mb);
  renderProcCount(frame.processes?.length ?? null);   // 缺省 = 未采样
  renderSessions(frame.agents || []);                 // 每 project 一条
  pushChartPoint(frame.container.cpu_percent, frame.container.memory_mb);

  // agent.alive 为 null 是"未知"，不要显示成"已退出"
  renderAgentAlive(frame.agent.alive);
});

es.onerror = () => {
  // 容器可能已死：不要只重连，去查 health
  checkHealth();
  setTimeout(() => es.close(), 0);
};
```

### 5.2 轮询健康 + 感知重启

```js
let last = null;
setInterval(async () => {
  const { data } = await (await fetch(`${BASE}/computer/pod/health?user_id=6`)).json();
  const h = data.health;

  if (last) {
    if (h.pod_uid && last.pod_uid && h.pod_uid !== last.pod_uid) {
      onContainerReplaced();               // 容器被重建（restart_count 会归零）
    } else if (h.restart_count > last.restart_count) {
      const t = h.last_termination;
      if (t?.is_oom) onOOM(t.finished_at, t.exit_code);
      else           onCrash(t?.reason, t?.exit_code);
    }
  }
  last = h;
}, 3000);
```

### 5.3 拉概览表

```js
const { data } = await (await fetch(`${BASE}/computer/pod/overview?events_limit=5`)).json();

renderSummary(data.summary);
renderTable(data.pods.map(p => ({
  name: p.name,
  identity: p.identity ? `${p.identity.service_type} · user ${p.identity.user_id ?? "-"}` : "系统",
  phase: p.phase,
  ready: p.ready,
  memLimit: p.spec.mem_limit_mb,
  memUsed: p.usage?.mem_used_mb ?? null,       // null = 采集不到
  cpuUsed: p.usage?.cpu_cores ?? null,
  restarts: p.health.restart_count,
  lastExit: p.health.last_termination?.reason ?? "—",
  isOom: p.health.last_termination?.is_oom ?? false,
  warnings: p.events.filter(e => e.event_type === "Warning").length,
})));

if (data.degraded.length) console.warn("部分分区降级:", data.degraded);
```

---

## 6. 常见坑（务必逐条核对）

| # | 坑 | 正确做法 |
|---|---|---|
| 1 | 用 `exit_code == 137` 判断 OOM | 用 `is_oom` / `reason == "OOMKilled"` |
| 2 | 用 `pod_name` 做长期关联键 | 用 `container_name`（跨重建不变） |
| 3 | `restart_count` 变 0 就以为"没重启过" | 先比 `pod_uid`，变了 = 容器被重建 |
| 4 | 把 `usage: null` 当 0 | 是"采集不到"，显示"—" |
| 5 | 把 `degraded: []` 当"没数据" | 看 `pods.length` |
| 6 | 把 `agent.alive: null` 显示成"已退出" | null = 未知 |
| 7 | 只靠 SSE 判断容器死活 | 容器死了流会断，用 `/computer/pod/health` |
| 8 | 把 `processes` 缺省当"0 个进程" | 缺省 = 未采样；`[]` 才是真的 0 |
| 9 | 对 health 高频无脑轮询 | 有 3s 缓存，3~5s 轮询即可 |
| 10 | 期待"历史 OOM 次数" | 数据源只保留最近一次 |

---

## 7. 错误语义

| 场景 | HTTP / code |
|---|---|
| 参数非法（如 `service_type` 非法值） | `ERR_VALIDATION`（400 类），带 `error_detail.stage` |
| **容器不存在** | **200 + `phase="NotFound"` + `source="none"`**（**不是 404**） |
| 采集失败（k8s API 不可用等） | `ERR_SERVICE_UNAVAILABLE`（503 类） |
| 目标类型不支持（如 userapp prod） | `ERR_OPERATION_NOT_SUPPORTED` |

> 容器不存在时 `data` 仍返回 `health` 对象（`alive=false`），前端据 `phase` 判断即可，不要用 `code != 0000` 判断"容器不在"。

---

## 8. 两种部署模式的行为差异

| | k8s | docker compose |
|---|---|---|
| OOM 后容器 | **自动重启**（kubelet） | **不重启**，尸体保留 |
| 死因可查 | 完整 | 容器保留期间可查；**约 10 分钟后被回收** |
| 需要业务侧重建 | 否 | **是**：检测 `phase=NotFound` → 调 `/computer/pod/ensure` |

内网测试环境是 **k8s**，按左列处理即可。

---

## 9. FAQ

**Q: 怎么知道某个容器现在是否 OOM 了？**
A: `health.last_termination.is_oom === true` 且 `restart_count` 相比上次增加。

**Q: `restart_count` 在 docker 模式下总是 0？**
A: 不一定。docker 的 `RestartCount` 只统计 restart policy 触发的重启（agent 容器没有
restart policy，这个字段原样透出时为 0）；rcoder 另外从 `docker events` 里数同一容器的
`die→start` 恢复对作为 `restart_count`，来源见 `restart_source="docker_events"`。agent 容器
停止后保留 10 分钟再回收（`RCODER_STOPPED_AGENT_RETENTION_SECS`），保留期内发生过的
恢复对都能数到，所以 `restart_count` 可能非 0。重建（新容器实例）用 `pod_uid` 变化识别。

**Q: 为什么 `overview` 里的 Pod 比 `kubectl get pods` 多/少？**
A: 数量差异来自**列表口径**（docker 下是本 daemon 全部容器；k8s 下是当前 namespace
全部 Pod，含未登记的系统组件），**不是** phase 过滤——`overview` 不做 phase 过滤，
`Succeeded` 的 Pod 会原样返回，只是不计入 `summary.not_ready`
（终态任务的 `ready=false` 是正常终态，不是异常）。
要按状态筛请在前端按 `phase` 过滤，或看 `summary` 计数。

**Q: `degraded[].detail` 是什么语言？**
A: 英文（可能带运行时的原始错误串）。响应有服务端缓存、跨请求复用，所以不按
`Accept-Language` 渲染；需要本地化文案时请按 `section` + `reason` 自行映射。

**Q: 想看某个 Pod 的事件明细？**
A: `overview.pods[].events`（数量由 `events_limit` 控制）。

**Q: SSE 断线了怎么办？**
A: 先查 `/computer/pod/health` 判断容器是否还在，再决定是重连还是提示用户容器已停止。

---

## 10. 联调建议顺序

1. 先调 `/computer/pod/health?user_id=6` —— 确认网络通、参数对
2. 再调 `/computer/pod/overview` —— 确认能拿到全量列表
3. 最后接 SSE —— 处理断线与重连

在线 Scalar 页面（`/api/docs/scalar`）支持直接填参数点"Send"，可用来快速验证。
