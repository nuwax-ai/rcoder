# Pod 观测接口 —— 对接指南

面向前端 / 调用方。只写"对接时必须知道"的东西：字段语义、坑、以及怎么用。

## 1. 接口清单

| 接口 | 用途 | 典型频率 |
|---|---|---|
| `GET /computer/pod/health` | **单容器健康**（存活、重启数、最近一次终止） | 3~5s 轮询 |
| `GET /computer/pod/overview` | **命名空间概览**（全部 Pod 的规格/健康/用量/事件） | 5~10s 轮询 或 打开页面时拉一次 |
| `GET /computer/pod/resources` | 单容器**实时资源**（CPU/内存/进程明细） | 2~3s 轮询 |
| `GET /computer/pod/resources/stream` | 同上，SSE 推送 | 长连接 |

**三者参数同源**：`service_type` + `app_id` + `app_stage`（与 `/computer/pod/ensure` 同一套分派），`user_id` / `project_id` / `pod_id` 用于定位。

查看实时定义：`GET /api/docs/openapi.json`（或 Scalar 页面）。

## 2. 容器标识：`container_name` vs `pod_name`

**这是最容易搞混的地方，务必分清。**

| 字段 | 含义 | 跨重建 | 用途 |
|---|---|---|---|
| `container_name` | **workload 名**：`rcoder-computer-agent-runner-6` | **不变** | 业务关联键（和 `overview.workload.name` 同值） |
| `pod_name` | **实际 Pod 名**：`rcoder-computer-agent-runner-6-0` | 可能变 | 排障、`kubectl logs`（和 `overview.items[].name` 同值） |
| `pod_uid` | 物理 ID（k8s: Pod UID；docker: 容器 ID） | **重建必变** | 判断"是否换了一代容器" |

对应关系（已实测验证）：

```
health.container_name  ==  overview.workload.name
health.pod_name        ==  overview.name
health.pod_uid         ==  overview.pod_uid
```

> docker 模式下 `pod_name` 与 `container_name` 同值（没有 Pod 概念）。

## 3. 最近一次终止：`last_termination`

**两个接口里结构完全一致**（`/computer/pod/health` 与 `/computer/pod/overview` 的 `health` 段）：

```json
"last_termination": {
  "reason": "OOMKilled",              // OOMKilled | Error | Completed | Unknown
  "exit_code": 137,
  "finished_at": "2026-11-18T04:07:26Z",
  "is_oom": true
}
```

规则：

- **`last_termination` 缺省（字段不存在）= 从未终止过**，不是空对象；
- **判断 OOM 只用 `is_oom`（或 `reason == "OOMKilled"`）**
  ⚠️ **绝不要用 `exit_code == 137` 推断** —— 137 同样来自 liveness 探针 SIGKILL 或人工 kill，实测两者都会出现，但只有后者 `is_oom=false`；
- `finished_at` 是**最近一次**终止时间，任何原因都有值（非 OOM 同样可读）；
- 历史"OOM 过几次"**拿不到** —— k8s 只保留最近一次（`lastState` 单槽位），这是数据源限制，不是接口缺陷。

## 4. 时间字段

所有时间都是 **RFC3339、秒精度**（如 `2026-11-18T04:07:26Z`）：

- `sampled_at` —— 本份数据的采集时刻（用来判断新鲜度）
- `started_at` —— 本次容器启动时间
- `last_termination.finished_at` —— 最近一次终止时间

`started_at` 与 `finished_at` 相邻比较可算出"这次重启花了多久"。

## 5. 缓存与 `fresh`

`/computer/pod/health` 默认有 **3 秒缓存**（键 = runtime + identifier）。

- 正常轮询：**不要传 `fresh`**，让缓存挡住重复查询
- 需要即时值（如用户手动点"刷新"）：`?fresh=true` 跳过缓存

`/computer/pod/overview` 有 **10 秒缓存**。

> 数据源本身有固有延迟：k8s 的 pod status 由 kubelet 周期汇报，端到端通常几秒。**别期待秒级实时**。

## 6. 典型用法

### 6.1 监控 OOM / 异常重启

```js
let last = null;
setInterval(async () => {
  const { data } = await (await fetch('/computer/pod/health?user_id=6')).json();
  const h = data.health;

  if (last) {
    // ① 容器被重建（restart_count 会归零，必须靠 pod_uid 识别）
    if (h.pod_uid && last.pod_uid && h.pod_uid !== last.pod_uid) {
      onContainerReplaced();
    }
    // ② 发生过重启
    else if (h.restart_count > last.restart_count) {
      const t = h.last_termination;
      if (t?.is_oom) onOOM(t.finished_at, t.exit_code);   // OOM
      else           onCrash(t?.reason, t?.exit_code);     // 其他原因
    }
  }
  last = h;
}, 3000);
```

### 6.2 容器不存在

`health.phase === "NotFound"` + `source === "none"`，`data` 仍返回 200（不是 404）。

此时 `last_termination` **可能仍有值**：如果容器刚消失且 Docker 事件历史还在窗口内，后端会尽量还原死因；还原不了就是 `null`。

### 6.3 命名空间大盘

```js
const { data } = await (await fetch('/computer/pod/overview')).json();
data.summary;   // { pods, running, not_ready, restarted, oom_killed_last, with_warning_events }
data.pods;      // 每项：name( Pod 名) / workload / identity / phase / spec / health / usage / events
data.degraded;  // 非空表示某些分区没取到（如 metrics 不可用），**不是失败**
```

- `degraded: []` = 全部分区成功；**空数组 ≠ 没数据**，要用 `pods.length` 判断；
- `usage` 缺省 = 采集不到（metrics-server 未装 / 容器未运行），**不是 0**；
- 排序：异常优先（运行中且未就绪 > OOM > 有重启），已完成的 Job 沉底。

## 7. 两种部署模式的行为差异（重要）

| | k8s | docker compose |
|---|---|---|
| OOM 后容器 | **自动重启**（kubelet） | **不重启**，容器尸体保留 |
| 死因可查 | `restart_count` + `last_termination` | 容器保留期间可查；**10 分钟后被回收**，之后只能靠 Docker 事件窗口（256 条 / 可能仅几分钟） |
| 需要业务侧重建 | 否 | **是**：检测到 `NotFound` 后调 `/computer/pod/ensure` |

docker 模式下如果不主动重建，容器就一直是"消失"状态。

## 8. 错误语义

| 场景 | HTTP / code |
|---|---|
| 参数非法（如 `service_type` 不是合法值） | `ERR_VALIDATION`（400 类） |
| 容器不存在 | **200 + `phase=NotFound`**（不是 404） |
| 采集失败（如 k8s list 挂了） | `ERR_SERVICE_UNAVAILABLE`（503 类），带 `error_detail.stage` |
| 目标不支持（如 userapp prod 阶段） | `ERR_OPERATION_NOT_SUPPORTED` |

## 9. 不要做的事

- ❌ 用 `exit_code == 137` 判断 OOM → 用 `is_oom`
- ❌ 用 `pod_name` 做长期关联键 → 重建后会变，用 `container_name`
- ❌ 把 `usage: null` 当 0 → 那是"采集不到"
- ❌ 把 `degraded: []` 当"没数据" → 看 `pods.length`
- ❌ 对 `/computer/pod/health` 高频无脑轮询 → 有 3s 缓存，2~3s 轮询即可
- ❌ 期待历史 OOM 次数 → 数据源不保留
