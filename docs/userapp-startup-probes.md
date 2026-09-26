# UserApp worker 启动检查

`kind = "worker"` 表示不公开代理路由。它不自动跳过启动检查，也不表示进程可以启动后退出。

## 选择模式

app-cli 0.3.9 新增可选字段；部署前必须确认运行中的 owner 在
`GET /v1/runtime/identity` 声明 `startup-probe-v1`。源码中具备能力不等于旧镜像已升级。

```toml
[project]
kind = "worker"
type = "python"

[run]
command = ["python3", "main.py"]

[health]
startup_probe = "process"
startup_timeout_seconds = 25
```

这是片段，完整项目仍需合法 service_id、build 和制品设置。

| startup_probe | 行为 |
|---|---|
| 省略 | 保持旧行为：builtin 的 dev + devrun 检查 TCP，其余进程检查 readiness_path 的 HTTP 2xx；supervisord 保持 startsecs 和既有 bridge |
| http | 两引擎都验证 readiness_path 的 HTTP 2xx；不在失败后降级为 TCP/process |
| tcp | 两引擎都验证已分配端口可以建立 TCP 连接 |
| process | 本次受管根进程连续存活 5 秒；早退（包括 exit 0）、观察错误或取消均不算成功 |

显式策略的预算从派发启动起计算，覆盖启动确认、检查与退避。builtin 在每个显式服务派发后立即检查，避免后续服务的慢迁移耗尽它的预算；省略策略的历史并行检查保持。supervisord 复用 `startProcess(wait=true)` 的 `startsecs=5` 证据，process 首次启动不使用十次隐式重试。提交启动成功前，两引擎再次核验同一执行实例。

process 只证明常驻进程存活，不能证明消息队列连接或业务初始化成功。程序应在初始化失败时非零退出，收到停止信号后收束工作。定时任务、执行完就退出的脚本应使用其他业务机制，不伪装为常驻 worker。

## 校验和兼容

- process 仅用于非 static 的 worker；不得声明 `[proxy]`、作为 workspace bridge 或被 `rcoder://` 上游引用。
- process 不声明 HTTP 健康路径；读取旧默认 `/health` 的值不触发兼容拒绝，但不使用这些路径。
- process 预算必须大于 5 秒；命令必须非空。
- static 使用平台内置托管，本次不支持显式 startup_probe；其源码 devrun 沿用既有检查。
- managed 模式仍需至少一个 web 服务，纯 worker workspace 在构建前明确拒绝。
- 未填写字段时序列化省略；这不代表新字段能被旧版本的严格解析器接受。
- 新字段的制品/源码能力预检在停止旧服务及目录激活之前执行；不兼容的请求失败时保留已有实例。
- worker 不参与 web 可访问性聚合；其启动失败仍使本次操作失败。未修改容器探针、PG 注入和错误页。

运行时、镜像及客户端写入链先升级，再发布默认输出新字段的 template-cli。存量项目不自动改写。HTTP worker 可以继续使用 `backend-python --kind worker`，保留其 FastAPI 入口；纯进程使用配套版本中的 `worker-python`。

## 验证入口

```bash
cargo nextest run -p workspace-manifest -p file-server -p file-server-userapp --no-fail-fast --all-features
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features
# 先构建配套 app-runtime；这两条真实 Docker 场景各覆盖 source + artifact。
make docker-build-app-runtime
make test-e2e E2E_SUITE=compose_userapp_faults E2E_FILTER=userapp_worker_
```

场景验证真实无监听 worker、HTTP 策略拒绝、成功启动、停止、重启、exit 0 失败和停止打断启动，并记录物理容器与 owner 身份。没有实际运行的环境不能以组件测试替代验收。

个人 K8s 的 `SUITE=userapp` 构建 fixture 包含一个无端口 worker，并核验制品中的策略、prod 冷/热启动时的真实进程及无监听事实。沿用生命周期、实际 HTTP、PVC 与跨副本检查。

Compose 的 `userapp_dev_server_lifecycle` 复用已有开发服务流程，额外搭配 process worker，验证 RCoder → agent_runner/file-server → 构建任务 → app-cli 的实际成功及停止收束：

```bash
make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_server_lifecycle
```

### 原生平台的小范围验证

```bash
python3 tools/native_worker_smoke.py \
  --binary /absolute/path/to/app-cli \
  --pingap /absolute/path/to/pingap \
  --root /absolute/path/to/isolated-test-directory
```

Windows 用 `python` 和 `.exe` 路径。需当前 app-cli 及匹配的 Pingap 0.14.3，不需要 Docker、数据库或 Node。脚本创建唯一子目录并保存操作回执和 `result.json`，使用 builtin 引擎，避免接管宿主机已有的 supervisord；3018/9080 已被占用时失败，不清理其他实例。supervisord 另由隔离 Docker 场景验证。

覆盖无监听进程启动、实际静态 HTTP、Stop/Start/Restart、提前退出失败及 owner 保留。API identity 可读不表示恢复完成，脚本先通过只读 recovery 接口确认初始化，再提交操作。Windows 最后结束自己空闲 owner 使用 TerminateProcess；不能据此声称 Windows 优雅关机已验证。此入口也不替代完整三平台、Compose 或 K8s 套件。
