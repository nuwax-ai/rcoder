# app-cli / file-server-proxy 本地监督与恢复

## 当前交付范围

两个 CLI 共用 `runtime-supervisor` Rust 库，不需要安装第三个守护服务。每个 CLI 有自己的监督父进程、执行进程和按需启动的命令 guardian；“单实例”指同一状态根只有一个有效执行者，不是整个应用只有一个操作系统进程。

当前实现覆盖 native 管理进程恢复、Docker builder 整体退出后的身份核验、容器回收前的有界收束，以及两类新的正向退出证据：进程空间纪元（宿主机重启/同容器硬杀重启）与 K8s Pod 更替的 API 核验回执。macOS、Linux、Windows 使用同一复合场景；源码检查、组件测试和实际部署验收分别记录。K8s 旧 Pod 仍存在或节点失联时保持保守保护（见下述边界），不能只凭组件测试升级生产镜像。

## 正常使用

```bash
app-cli serve --workspace /path/to/workspace
app-cli owner status --workspace /path/to/workspace
app-cli owner stop --workspace /path/to/workspace --request-id stop-20260927
app-cli owner recover --workspace /path/to/workspace --request-id recover-20260927
app-cli owner shutdown --workspace /path/to/workspace --request-id shutdown-20260927

file-server-proxy start --embed --policy all_rust
file-server-proxy status
file-server-proxy stop --request-id proxy-stop-20260927
```

app-cli 的 `owner stop` 停止业务执行，并恢复管理入口到已停止状态；随后可显式启动业务。`owner recover` 收束旧执行进程，再恢复管理入口，业务能否继续由既有 journal 决定。`owner shutdown` 结束该 CLI。停止 file-server-proxy 不等于停止 app-cli，即使后者最初由 proxy 启动。

CLI `owner` 命令返回包含 `operation_id`、`generation`、`supervisor_id` 和 `phase` 的 JSON；有恢复缺口时还包含结构化 `problem.code/message`。受理的 `stopping` 不是完成；使用同一请求 ID 和参数重试可读取原结果。不要把一次退出码 0 当成业务停止已经完成。相同 ID 改参数会拒绝；不同控制请求在已有用户控制尚未完成时返回 Busy，不做内部排队。

业务操作入口与上述监督控制命令不同：新 Start/Restart 可以替换单个待执行意图；旧操作明确失败或取消后继续执行新的请求。Stop 执行期间收到的新启动在停止完成后执行，旧 Stop 不会再停止新服务。尚未确认的进程收束仍由独立监督器处理。

状态根应位于固定、可写且不随部署目录替换的位置。app-cli 沿用 `APP_CLI_STATE_ROOT`/原工作区解析；proxy 沿用 `FILE_SERVER_PROXY_STATE_DIR`。不能删除或换名 `owner.lock` 来解除占用。

## 如何恢复挂起

1. 监督层通过独立 loopback 通道发带 nonce 的控制探测。app-cli 探测实际控制循环，慢构建、PG 等待、业务尚未 Ready 不直接判为进程挂起。
2. 连续失败达到预算后，先关闭该代次的新命令受理，再请求优雅停止。显式 Stop 走相同机制。
3. app-cli 最多给 3 秒优雅退出宽限，不接受执行进程延长宽限；随后 root guardian 使用保留的真实 Child 强制停止执行进程，每条受管命令的 guardian 用原进程组/Windows Job 收束进程树。builtin 和 supervisord 的业务服务也并发停止，单服务宽限最多 3 秒。进程退出后的回执、外部引擎确认与新管理进程初始化另有观察预算，不等于继续允许旧进程运行。
4. 旧代次及命令收束回执确认后，才启动新执行代次。未知数据库迁移、部署切换结果不因物理进程退出而改成成功或自动重跑。

父监督进程意外退出时，其专用 pipe 关闭，guardian 继续完成清理。下一次启动/停止可接续原控制记录。PID 仅用于诊断和启动握手，不是事后杀进程的凭据；不通过进程名、端口号猜测清理对象。

监督父进程使用 2 个 Tokio 工作线程，guardian 使用轻量单线程 runtime；业务执行进程沿用原调度配置。Windows 回执原子替换遇到临时共享冲突最多重试 500ms，失败保留旧文件并上报。命令的原回执在共享文件系统上暂时不可见时，启动/退出观察最多重读 250ms，仍持有原 Child；持续缺失或内容损坏明确失败，不生成替代命令、不推断退出成功。

默认控制探测每 2 秒一次，连续至少三次且无响应持续 15 秒才进入收束；首次管理初始化有 30 秒观察预算。自动恢复限额为 10 分钟内三次。睡眠或长调度间隔后重新采样，不累计补发过期心跳。

## 明确边界

- 收束过程返回 `cleanup_pending` 时，仍可通过独立控制通道查询。不要将其当成业务 Ready。
- 旧版没有监督回执的挂起进程，不能升级后凭 PID 自动接管；保留已有身份诊断与处置要求。
- Docker 新建/重建的受管理 builder 会记录 Docker daemon 身份、独立执行域标记和原数据挂载指纹。平台确认旧执行域的容器已经删除、同一 daemon 上没有仍可启动的旧容器，且新容器数据绑定一致后，通过捕获的容器 ID 写入对应监督代次的物理退出回执。原运行代次与命令才能退役；迁移结果、发布 journal 和业务期望状态不改成成功。
- 旧版无执行域标记的容器不补造退出证据。K8s 在闲置回收前最多等待 8 秒管理收束（内部观察为 7 秒），随后只删除预先捕获的资源 UID；收束失败不阻止物理停止。更换 Pod UID、端口拒连或取得文件锁单独均不是退出证明，不应清空状态目录绕过。
- **进程空间纪元证明（两 CLI 共用）**：每个执行代次记录其 OS 引导/PID 命名空间身份（Linux 为 `/proc/1/stat` 启动时刻，macOS/Windows 为引导时间推导）。在持有 owner 锁且无清理回执时，纪元变化本身就是本地正向证据——宿主机断电重启、容器内整体硬杀后重启（同一 Docker 容器或同一 K8s Pod 的容器重启）都在此列，监督进程据此收束旧代次；业务 journal、迁移结果与退出码不受影响。该证明只对「本进程空间内」的代次生效：带有其他容器/Pod 执行域标记的记录仍必须走平台核验。
- **K8s Pod 更替的核验回执**：builder Pod 注入执行域（集群身份 + per-app PVC 挂载指纹，instance 解析自 `RCODER_PHYSICAL_POD_UID`）。Pod 重建后（ensure/wake/restart），平台核验「旧 Pod UID 已从 API 消失，且集群没有 NotReady/Unknown 节点（活 kubelet 已收割被删 Pod 的进程）」才向新容器签发物理退出回执。旧 Pod 对象仍存在（含 terminating）、节点失联或 nodes 只读权限缺失（需在 RBAC 授予 `nodes get/list/watch`）时保留 `recovery_required` 并给出具体原因；管理面、文件访问与物理停止不受影响。
- supervisord 引擎收束先撤回动态配置，再停止和移除 `app-svc-*`/`app-pingap` 组，并复核进程表。配置撤回或 reload 失败时仍尝试停止动态组，但保留失败结果。worker 在第一次外部变更前记录所用引擎；该引擎的 socket 消失不能被视作 builtin 清理成功。root guardian 在 worker 与命令退出后执行同一清理适配器，外部清理未确认时不提交 Quiescent；固定 PG/dbx/ttyd 保留。
- `cleanup_pending` 表示仍在收束；`recovery_required` 表示当前缺少继续恢复所需的证据。file-server Start/Stop/Recover 同时查询独立监督通道，返回监督代次、操作和原因；不能只轮询公共 3010 并给出泛化超时。仍在清理的状态在同一总预算内等待。
- 私有监督控制协议现为 v2（结构化错误）。磁盘 v1 记录可读，新旧活动监督进程不混用私有控制协议；部署时两个 CLI、agent_runner 与 RCoder 需要配套更新并重启旧管理父进程。公开业务协议版本没有因此改变。
- 外层系统服务管理器必须给内部收束留出预算，不能用无条件 autorestart 抵消显式 shutdown。部署仓的 app-cli 与 proxy program 已调整为 `autorestart=unexpected`、`exitcodes=0`、`startsecs=0`，并留出 90 秒外层停止预算；仍需随配套镜像实测。
- 正常 HTTP/SSE 仍由执行进程提供；换代期间可能短暂断连，不重放用户 POST。

## 快速复合验收

需要 Python + psutil、与 app-cli 契约一致的 Pingap；不需要 Node、PG 或 LLM。固定 9080/3018 必须空闲。不要与占用这些端口的 app-cli 单元测试并发运行。

```bash
make native-runtime-build
make test-native-runtime NATIVE_PYTHON=/path/to/python \
  NATIVE_PINGAP=/path/to/pingap NATIVE_REPORT_ROOT=tmp/native-runtime
```

也可将 `NATIVE_PINGAP` 指定为 PATH 中的命令名。测试入口不自动构建镜像或安装环境；两个二进制显式构建后复用。

同一测试依次覆盖真实 app-cli 启动、文件服务 Stop、重复控制、挂起后强制收束、原监督退出后的管理恢复、proxy 重新拉起独立 app-cli、proxy 被杀/停止时 app-cli 继续运行，以及工作区标记保留。报告包含二进制 SHA-256、平台和每条断言，输出到指定目录的独立用例子目录。

本场景是 native 进程与 HTTP 复合验证，不替代完整 UserApp 构建部署、未知迁移恢复、Compose/K8s 或容器整体断电场景。

## 容器闲置回收回归

构建包含同一源码版本的 RCoder、agent_runner、app-cli 的测试镜像后执行：

```bash
E2E_SUITE=compose_userapp_dev \
E2E_FILTER=userapp_dev_idle_recycle_owner_recovery make test-e2e
```

可用 `E2E_IDLE_RCODER_IMAGE` 和 `E2E_IDLE_BUILDER_IMAGE` 指定隔离镜像。该场景由真实闲置回收器完成两次回收：第一轮挂起捕获的 guardian，强制验证原卷上未完成代次通过物理退出证据恢复；第二轮验证优雅退出后首次 Stop、再次 Restart。断言包含真实构建计数、HTTP 新内容、原生命周期/卷/文件保留。它不代替 K8s 节点失联或 RBD 挂载验收。
