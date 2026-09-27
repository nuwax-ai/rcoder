# app-cli / file-server-proxy 本地监督与恢复

## 当前交付范围

两个 CLI 共用 `runtime-supervisor` Rust 库，不需要安装第三个守护服务。每个 CLI 有自己的监督父进程、执行进程和按需启动的命令 guardian；“单实例”指同一状态根只有一个有效执行者，不是整个应用只有一个操作系统进程。

当前实现面向 native 故障恢复。macOS、Linux、Windows 使用同一复合场景验证，具体源码与实机记录见本轮交付说明。**容器整体强杀/宿主机重启的恢复交接尚未闭环，暂不能据此升级容器镜像或宣布完整部署验收。**

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

CLI `owner` 命令返回包含 `operation_id`、`generation` 和 `phase` 的 JSON。受理的 `stopping` 不是完成；使用同一请求 ID 和参数重试可读取原结果。不要把一次退出码 0 当成业务停止已经完成。相同 ID 改参数会拒绝；不同控制请求在已有用户控制尚未完成时返回 Busy，不做内部排队。

状态根应位于固定、可写且不随部署目录替换的位置。app-cli 沿用 `APP_CLI_STATE_ROOT`/原工作区解析；proxy 沿用 `FILE_SERVER_PROXY_STATE_DIR`。不能删除或换名 `owner.lock` 来解除占用。

## 如何恢复挂起

1. 监督层通过独立 loopback 通道发带 nonce 的控制探测。app-cli 探测实际控制循环，慢构建、PG 等待、业务尚未 Ready 不直接判为进程挂起。
2. 连续失败达到预算后，先关闭该代次的新命令受理，再请求优雅停止。显式 Stop 走相同机制。
3. 超过已协商的清理预算后，root guardian 使用保留的真实 Child 收束执行进程；每条受管命令的 guardian 用原进程组/Windows Job 收束进程树。
4. 旧代次及命令收束回执确认后，才启动新执行代次。未知数据库迁移、部署切换结果不因物理进程退出而改成成功或自动重跑。

父监督进程意外退出时，其专用 pipe 关闭，guardian 继续完成清理。下一次启动/停止可接续原控制记录。PID 仅用于诊断和启动握手，不是事后杀进程的凭据；不通过进程名、端口号猜测清理对象。

监督父进程使用 2 个 Tokio 工作线程，guardian 使用轻量单线程 runtime；业务执行进程沿用原调度配置。Windows 回执原子替换遇到临时共享冲突最多重试 500ms，失败保留旧文件并上报。

默认控制探测每 2 秒一次，连续至少三次且无响应持续 15 秒才进入收束；首次管理初始化有 30 秒观察预算。自动恢复限额为 10 分钟内三次。睡眠或长调度间隔后重新采样，不累计补发过期心跳。

## 明确边界

- 收束过程返回 `cleanup_pending` 时，仍可通过独立控制通道查询。不要将其当成业务 Ready。
- 旧版没有监督回执的挂起进程，不能升级后凭 PID 自动接管；保留已有身份诊断与处置要求。
- 若整个容器、宿主机或 guardian 同时被强杀，可能没有正向清理回执。当前会保留 `cleanup_pending`。**更换 Pod UID、PID namespace、端口拒连或取得文件锁，都不会自动使旧代次变为已清理。**后续需接入运行时确认原物理实例已退出的交接；不能清空整个状态目录绕过。
- supervisord 引擎的动态 program 是另一种进程归属，尚需完成独立适配及 Compose/K8s 验收；固定 PG/dbx/ttyd 不属于 app-cli 业务 Stop 的清理范围。
- 外层系统服务管理器必须给内部收束留出预算，不能用无条件 autorestart 抵消显式 shutdown。本轮未修改部署仓的外层配置。
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
