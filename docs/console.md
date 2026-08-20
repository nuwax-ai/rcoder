# tokio-console 本地观测指南

tokio-console 是 tokio 官方的异步运行时观测工具：任务级耗时排行、锁等待、
唤醒风暴、任务泄漏——对死锁类问题（如 activate 自死锁）提供运行时铁证。
**仅本地开发 feature（`console`），release 构建零代码零开销。**

## 架构

- `console-subscriber`（嵌入端，feature `console`）：rcoder 与 agent_runner
  都已接入，经 tracing layer 注入（独立于 file-server 嵌入占用的 extra_layer 槽），
  bind `0.0.0.0:6669`（`CONSOLE_BIND` env 可配）
- `tokio-console`（TUI 客户端）：宿主机连接观测
- 编译前提：`RUSTFLAGS="--cfg tokio_unstable"`（tokio 官方为 console 保留的
  插桩开关，不改运行语义）——**所有封装命令已自动携带**
- 缓存隔离：console 构建用独立 target（本地 `target-console/`、容器
  `rcoder-target-console` volume）——RUSTFLAGS 指纹与普通构建互不污染

## 启用方式

### rcoder（docker compose 本地）

```bash
DEV_CONSOLE=1 make dev-hot     # 首次全量编译约 10 分钟（独立 target volume；此后增量秒级）
tokio-console http://localhost:6669   # 注意 http:// 前缀（TUI 0.1.x 要求显式 scheme）
```

切回普通模式：`make dev-hot`（不带 DEV_CONSOLE，各自缓存独立零重编）。

### rcoder（本地 cargo run）

```bash
make run-console              # = RUSTFLAGS + target-console + --features console
tokio-console http://localhost:6669
```

### agent_runner（动态 agent 容器）

构建带 console 的 agent 镜像后，动态容器即有观测（bind 0.0.0.0，宿主机
经容器 IP 直连——OrbStack 特性）：

```bash
AGENT_CONSOLE=1 make docker-build-agent-runner
# 起一个对话让 agent 容器运行，然后：
docker inspect -f '{{.NetworkSettings.Networks.rcoder_default.IPAddress}}' <agent容器名>
tokio-console http://<容器IP>:6669
```

### K8s（devspace / 测试集群）

构建参数同上透传；连接用 port-forward：

```bash
kubectl -n <ns> port-forward <rcoder-pod> 6669:6669
tokio-console http://localhost:6669
```

## TUI 常用操作

| 按键 | 作用 |
|---|---|
| `t` / `r` / `k` | 任务(task) / 资源(锁等) / 产生者(spawn sites) 视图 |
| `w` | 按 wake 时间排序（找唤醒风暴） |
| `p` / `P` | 按 poll 时间排序（找长任务） |
| `l` | 按锁等待排序（死锁排查主视图：`lock` 资源的 `since` 持续增长 = 疑似死锁） |
| `f` | 过滤（如 `rcoder::` 前缀过滤自己模块的任务） |
| `q` | 退出 |

死锁排查套路：`r` 进资源视图 → `l` 按持有时间排序 → 持有 `since` 持续
增长的锁 + `t` 视图里卡在 `lock` 状态的任务 = 死锁对。

## 与日志级别（RUST_LOG）的关系

tokio 任务/waker 事件为 **trace 级**——EnvFilter 全局压制会挡住 console 层
（现象：TUI 连上但零任务）。telemetry 已在 console 开启时自动给 EnvFilter
叠加 `tokio=trace` + `runtime=trace`（精准放行，两 target 无业务日志，
不影响 fmt/文件层输出），**无需任何 RUST_LOG 配置**。

- 全局 `RUST_LOG=trace` 也能让 console 工作，但 hyper/tonic/bollard 等
  依赖的 trace 日志海量，仅适合临时深挖，不建议常驻
- 临时查某个库用精准指令：`RUST_LOG=debug,bollard=trace`

## 约束

- release / CI 构建不带 feature、不带 RUSTFLAGS——二进制不含 console 代码
- 开启时内存开销：任务事件环形缓冲（默认容量级别，本地可接受）
- console-subscriber 0.5；上游源码参考：本地 git-workspace/console
