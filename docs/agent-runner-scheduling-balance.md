# agent-runner 调度均衡方案(topologySpreadConstraints,待实施)

> 状态:已记录,待实施(优先级:中,drain 后视情况)
> 日期:2026-08-13
> 关联:今晚 drain 19 会自然缓解(迁移分散),本方案防止以后再扎堆

## 现状(2026-08-13 发现)

computer-agent-runner **25 个全堆 .19**,.34 有 6 个,**.13 有 0 个**。实际 CPU:19=30%、34=7%、**13=2%(最闲却 0 个)** —— 反常。

## 根因:requests 虚标导致调度器失明

agent-runner requests 极低(**cpu 5m / mem 64Mi**),实际 CPU **100~1200m**(VNC 桌面 + agent 工作)。

- **来源**:`build_decoupled_resources`(`crates/docker_manager/src/runtime/k8s_app_helpers.rs:196`)
- **有意设计**:低 requests(5m) + 高 limits(4Gi/2cpu) + swap,让 pod 超用闲置 CPU(CPU 可压缩,超 requests 只 throttle)+ memory swap 兜底(不易 OOM)
- **副作用**(注释没预见的):25 个 requests 才 250m,调度器按 requests 以为很轻,全堆 19(19 实际 ~9000m)。`build_decoupled_resources` 注释只考虑了"单 pod 超用资源"和"throttle/evict",没考虑调度失衡
- 注释原文风险:"严重争抢时 throttle(healthcheck 失败),必要时调大(50m)"、"内存紧张时被 evict"

## 方案 A:topologySpreadConstraints(推荐,不改 requests)

在 `build_agent_pod_spec`(`crates/docker_manager/src/runtime/k8s_agent_create.rs:113`)按 service_type 注入:

```yaml
topologySpreadConstraints:
  - maxSkew: 1                              # 任意两节点 pod 数差 ≤ 1
    topologyKey: kubernetes.io/hostname     # 按节点分散
    whenUnsatisfiable: ScheduleAnyway       # ★ 不阻断业务(不用 DoNotSchedule,否则某节点满会 Pending)
    labelSelector:
      matchLabels:
        app.kubernetes.io/name: computer-agent-runner   # 所有 computer-agent-runner(跨 STS)
```

**参数理由**:
- `maxSkew=1`:严格均衡,25 个/3 节点 → ~8 个/节点
- `whenUnsatisfiable=ScheduleAnyway`:不阻断 agent-runner 创建(关键)
- `labelSelector` 用 `app.kubernetes.io/name=computer-agent-runner`(所有 computer-agent-runner 共享,跨 STS),调度器算 25 个的总分布

**web-agent-runner 同理**:`labelSelector` 用 `app.kubernetes.io/name=web-agent-runner`,按 service_type 分别设。

## 代码改法(约 15 行)

`build_agent_pod_spec` 构造 pod_spec 后,按 service_type 设 labelSelector 注入:

```rust
let spread_label = match service_type {
    ComputerAgentRunner => "computer-agent-runner",
    WebAgentRunner => "web-agent-runner",
    _ => return pod_spec,
};
pod_spec.topology_spread_constraints = Some(vec![TopologySpreadConstraint {
    max_skew: 1,
    topology_key: "kubernetes.io/hostname".into(),
    when_unsatisfiable: ScheduleWhenUnsatisfiable::ScheduleAnyway,
    label_selector: LabelSelector {
        match_labels: [("app.kubernetes.io/name".into(), spread_label.into())].into(),
    },
    ..Default::default()
}]);
```

不改 requests、不改 config、不碰 java。

## 效果

- 新 agent-runner 往 pod 最少的节点调度,均匀分散
- **现有 pod 不迁移**(topologySpread 只影响新调度)
- 今晚 drain 19 是契机:迁移的 25 个重新调度时,加上 topologySpread 会均匀分散到 13/34(不加则按调度器自然选择,13/34 都比 19 闲,也会分散但无保证)

## 备选:方案 B(调大 requests)

按 `build_decoupled_resources` 注释建议,requests cpu 5m → 200~500m(接近实际)。调度器感知真实负载,自然分散。代价:限制能开的 agent-runner 数(调度额度)。

**不推荐 B**(破坏"超用闲置资源"设计),推荐 A(topologySpread 强制均衡,保持低 requests 设计)。
