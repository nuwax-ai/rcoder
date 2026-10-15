# app-cli 使用指南

app-cli 是 UserApp 的跨平台构建与服务管理器，通过当前工作区的 manifest、Pingap 和管理 API 编排服务。它是[独立 Cargo 项目](../../crates/app-cli/Cargo.toml)，通过 npm 包 `@nuwax-ai/app-cli` 分发。

## 安装与前置

```bash
npm install -g @nuwax-ai/app-cli
app-cli --help
app-cli run --help
```

npm wrapper 选择当前平台二进制及配套 Pingap。运行应用前，还须准备 manifest 声明的语言运行时、启动命令与服务依赖。平台镜像内的安装与宿主机 npm 安装分别生效。包结构与发布机制见 [npm 说明](../../npm/README.md)。

## 选择命令

| 命令 | 用途 |
|---|---|
| `validate` | 只读检查源码 manifest、依赖、端口与代理配置，不构建、不启动 |
| `gen-lock` | 校验 manifest 并生成 `release.lock.toml`，不构建、不启动 |
| `build` | 执行构建，可用 `--deploy-dir` 生成部署目录，不启动服务 |
| `run` | 向常驻管理 owner 提交当前源码启动请求，等待本次操作结果 |
| `serve` | 常驻管理入口，处理启动、停止、重启与部署；`--control-only` 仅恢复管理入口 |
| `readiness` | 只读查询实际业务就绪状态，不启动管理器或业务 |
| `owner status/stop/recover/shutdown` | 经独立监督通道查询、停止业务、恢复管理或结束 CLI |
| `run-service` | supervisord 使用的内部服务入口 |
| `journal adopt` | 部署日志冲突的管理命令；日常启动使用 `run` |

参数放在子命令后。工作区按 `--workspace`、`APP_CLI_WORKSPACE`、当前目录依次选择，相对路径基于调用命令时的工作目录。下面示例先取得已有项目的绝对路径，后续即使切换目录也使用同一应用身份。容器默认日志目录为 `/app/logs`；宿主机运行前，应将 `APP_CLI_LOG_DIR` 或 `--log-dir` 指向当前用户可写的目录。管理监听默认 `0.0.0.0:3010`，只读 readiness 查询默认 `127.0.0.1:3010`；本机示例显式使用 loopback，后续命令保持同一管理配置。

## 当前源码启动与重复运行

```bash
app_workspace="$(cd ./my-app && pwd)" || exit 1
export APP_CLI_LOG_DIR="$PWD/my-app-logs"
export APP_CLI_ADMIN_ADDR="127.0.0.1:3010"
app-cli validate --workspace "$app_workspace" --dev
app-cli run --workspace "$app_workspace"
# 修改源码或修正配置后，使用同一入口再次启动
app-cli run --workspace "$app_workspace"
```

普通 `run` 复用经过核验的 `serve` owner；没有可用 owner 时，自动启动独立的 `serve --control-only` 管理进程，再提交本次请求。CLI 等待自己的操作到达终态后返回；管理进程继续存活，业务 Stop 或一次启动失败也保留管理、停止和日志能力。

源码请求使用当前 manifest，优先选择 `[devrun]`，缺失时使用 `[run]`。源码需要的编译步骤由项目启动命令及平台构建流程承担；`run` 不调用独立的 `build` 子命令。需要生成可部署产物时，使用下一节的显式构建流程。显式 `APP_DEPLOY_*` 制品输入沿制品部署通道处理。

重复运行、停止后再运行、修正失败源码后再运行，以及 owner 退出后的恢复，都使用同一入口。旧锁文件、历史 PID 或 `Running` 记录本身不代表进程存活。恢复由 owner 协调旧执行停止与新请求，保持同一工作区最终只有一套业务服务；不要删除锁文件、journal 或另起一套服务来解除占用。

请求已受理但结果未知时，应按输出的原 `operation_id` 查询。操作失败与业务就绪分别判断，管理端口可达不代表业务已经启动成功。详情见 [owner 恢复](../userapp-dev-owner-recovery.md)、[错误与重试](../runtime-error-contract.md)。

## 构建并运行制品

先离线组装制品。`--deploy-dir` 的相对路径也基于调用时目录，下面显式使用绝对路径；构建本身不切换已经运行的 owner 或业务。

```bash
app_workspace="$(cd ./my-app && pwd)" || exit 1
app_deploy_dir="$PWD/my-app-deploy"
export APP_CLI_LOG_DIR="$PWD/my-app-logs"
export APP_CLI_ADMIN_ADDR="127.0.0.1:3010"
app-cli validate --workspace "$app_workspace"
app-cli gen-lock --workspace "$app_workspace"
app-cli build --workspace "$app_workspace" --deploy-dir "$app_deploy_dir"
```

首次独立运行这份制品时，先确认该应用没有仍在运行的管理 owner 和业务。若已执行上一节的源码 `run`，管理进程仍在后台；通过原工作区执行 `app-cli owner shutdown --workspace "$app_workspace"`，保留返回的 `operation_id`，按下一节说明观察同一请求，确认停止完成、原 owner 退出及管理端口释放后再继续。不能把业务 Stop 当作管理 owner 已退出，也不能给新进程换端口来绕开它。平台托管应用通过现有部署接口切换制品，不在容器中另起一套管理器。

满足上述前置后，在终端中启动制品管理器：

```bash
app-cli serve --workspace "$app_deploy_dir"
```

`build --dev` 选择开发构建步骤，不能与 `--deploy-dir` 混用；`gen-lock --dev` 预览开发代理策略，lock 中仍保留开发与生产设置。`serve` 是前台常驻命令，后台托管由调用方负责；控制命令在另一个终端执行，并传入相同的日志、管理配置和制品工作区。Pingap 业务入口默认 9080，以实际参数和清单为准。

## 查询与停止

```bash
app-cli readiness --admin-addr 127.0.0.1:3010 --json
app-cli owner status --workspace "$app_workspace"
app-cli owner stop --workspace "$app_workspace"
# 停止后再次运行当前源码
app-cli run --workspace "$app_workspace"
```

本节示例控制前面的源码工作区；若管理器运行制品目录，查询和控制时使用对应制品工作区，保持同一身份。`owner stop` 停止业务并保留管理入口；`owner recover` 收束旧执行并恢复管理；`owner shutdown` 结束整个 CLI。

省略 `--request-id` 时，CLI 为本次新控制操作生成唯一 ID。保存响应中的 `operation_id`；重试或等待这同一次操作时，用 `--request-id` 传回该 ID，并保持动作与其他参数一致。启动后的新一次 Stop/Recover/Shutdown 使用新 ID，不能重复使用教程中的固定字符串，否则可能读到较早操作的结果。`stopping`、`cleanup_pending` 或取消受理不代表清理完成；沿原 ID 继续观察到确认终态，再进行依赖停止完成的后续操作。

## 数据库凭据与运行操作

修改 PostgreSQL 密码本身不请求停止业务、重启服务或替换容器。凭据更新与业务运行操作分别处理；业务何时重启由用户决定。启动时使用当前明确提供的有效凭据，内部恢复记录的处理见[迁移与凭据策略](../userapp-migration-policy.md)。

当前源码启动以有效 manifest、实际 owner 和执行状态为依据，退役的 source-seal 文件不再是运行前置。保留项目的正常状态目录及历史记录，继续使用 `app-cli run` 完成启动或恢复。

## 相关指南

- [配置校验与退出码](app-cli-validate.md)
- [本地监督与恢复](../deployment/native-supervision.md)
- [UserApp 子路径与代理配置](../concepts/userapp-subpath-routing.md)
- [启动探针](../userapp-startup-probes.md)、[业务就绪](../userapp-readiness-container.md)
- [迁移策略](../userapp-migration-policy.md)、[停服日志](../userapp-stopped-logs.md)
