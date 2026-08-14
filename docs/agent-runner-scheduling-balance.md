# agent-runner 调度均衡方案(topologySpreadConstraints)

> 状态:**已实施**(2026-08-13)。见 `build_agent_pod_spec`。
> 日期:2026-08-13
> 关联:drain 19 会自然缓解(迁移分散),本方案防止以后再扎堆

## 现状(2026-08-13 发现)

computer-agent-runner **25 个全堆 .19**,.34 有 6 个,**.13 有 0 个**。实际 CPU:19=30%、34=7%、**13=2%(最闲却 0 个)** —— 反常。

## 根因:requests 虚标导致调度器失明

agent-runner requests 极低(**cpu 5m / mem 64Mi**),实际 CPU **100~1200m**(VNC 桌面 + agent 工作)。

- **来源**:`build_decoupled_resources`(`crates/docker_manager/src/runtime/k8s_app_helpers.rs:196`)
- **有意设计**:低 requests(5m) + 高 limits(4Gi/2cpu) + swap,让 pod 超用闲置 CPU(CPU 可压缩,超 requests 只 throttle)+ memory swap 兜底(不易 OOM)
- **副作用**(注释没预见的):25 个 requests 才 250m,调度器按 requests 以为很轻,全堆 19(19 实际 ~9000m)。`build_decoupled_resources` 注释只考虑了"单 pod 超用资源"和"throttle/evict",没考虑调度失衡
- 注释原文风险:"严重争抢时 throttle(healthcheck 失败),必要时调大(50m)"、"内存紧张时被 evict"

## 方案 A:topologySpreadConstraints(已实施)

在 `build_agent_pod_spec`(`crates/docker_manager/src/runtime/k8s_agent_create.rs`)按 service_type 注入:

```yaml
topologySpreadConstraints:
  - maxSkew: 5                              # ScheduleAnyway 下数值几乎不影响结果(见下文)
    topologyKey: kubernetes.io/hostname     # 按节点分散
    whenUnsatisfiable: ScheduleAnyway       # ★ 软约束,绝不阻断 agent-runner 创建
    labelSelector:
      matchLabels:
        app.kubernetes.io/name: computer-agent-runner   # 所有 computer-agent-runner(跨 STS)
```

**参数理由**:
- `maxSkew=5`:见下文「maxSkew 取值」——ScheduleAnyway 下具体值几乎不影响结果,5 只是
  "允许一定倾斜"的语义,填 2/5/20 实际行为一致。
- `whenUnsatisfiable=ScheduleAnyway`:不阻断 agent-runner 创建(关键,见下文)。
- `labelSelector` 用 `app.kubernetes.io/name=<service_type>`(与 `build_standard_labels`
  写入的 label 一致),调度器算同类 agent-runner 的总分布。

**web-agent-runner 同理**:`labelSelector` 用 `app.kubernetes.io/name=web-agent-runner`,按 service_type 分别设。

### maxSkew 取值:ScheduleAnyway 下几乎不影响结果(填 5)

⚠️ 经查 K8s 官方文档与调度器实现:**maxSkew 只在 `whenUnsatisfiable=DoNotSchedule` 时
作为硬过滤阈值**。一旦用 `ScheduleAnyway`(本方案):

- maxSkew **不作为硬约束执行**,pod 总会被调度(不阻断)。
- 调度器在打分阶段**永远优先 pod 最少的节点**(least-occupied),**与 maxSkew 的具体数值
  基本无关**。
- 所以填 2 / 5 / 20,**实际调度行为几乎一模一样**——都是"往最闲节点放 + 绝不阻断"。
  maxSkew 在 ScheduleAnyway 下基本是装饰性的。

→ 取 5,表示"允许一定倾斜"的语义。要真正强制硬均衡(差超过 N 就不让放)必须改用
`DoNotSchedule`,但那会 Pending 业务,本场景不采用。

### whenUnsatisfiable 取值:为什么是 ScheduleAnyway(不是 DoNotSchedule)

- `ScheduleAnyway`(采用):即使放到任何节点都违反 maxSkew,也照常调度,只是优先选能
  减小 skew 的节点。agent-runner 是用户实时创建的业务 pod,**绝不能 Pending/创建失败**。
- `DoNotSchedule`(不用):不满足 maxSkew 就 Pending。某节点被 cordon / 资源不足 /
  两个节点都满了时会卡住 agent 创建 → 业务故障。

## 效果

- 新 agent-runner 往 pod 最少的节点调度,均匀分散
- **现有 pod 不迁移**(topologySpread 只影响新调度)
- 今晚 drain 19 是契机:迁移的 25 个重新调度时,加上 topologySpread 会均匀分散到 13/34(不加则按调度器自然选择,13/34 都比 19 闲,也会分散但无保证)

## 配套改动:requests cpu 5m → 50m

与 topologySpread 同期落地(`build_decoupled_resources`)。这是注释里早就建议的
"必要时调大(如 50m)"防 CPU 饿死最低值,只解决 cpu.shares 权重过低导致的深度 throttle
(healthcheck 失败/启动超时),**不解决调度均衡**(50m 仍很低,调度器仍按"节点很轻"看待)。
均衡完全靠上面的 topologySpread。两改动互补、不冲突。

未直接跳到 200~500m(接近实际峰值)的原因:那会大幅消耗调度额度,限制能开的 agent-runner
数量,破坏"低 requests + 高 limits 超用闲置 CPU"的设计意图。
