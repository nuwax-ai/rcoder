# pnpm 构建脚本审批策略（file-server）

本文说明 file-server 托管的 pnpm 安装流程、配置修改范围及故障处理。实际运行版本应在执行安装的同一容器或宿主机中用 `pnpm --version` 核对；本文不规定 pnpm 的全局版本，也不把源码行为当作运行镜像已经验证的证据。

## 安装入口与策略

四条路径最终调用 [pnpm 安装服务](../src/service/pnpm/mod.rs)，安装前的配置准备各有边界：

| 路径 | 当前调用位置 | 配置准备与构建脚本策略 |
|---|---|---|
| `install-project` 的 TypeScript 分支 | [handlers/computer/packages.rs](../src/handlers/computer/packages.rs) → [ops/packages.rs](../src/ops/packages.rs) 的 `install_project_core` | 调用 `ensure_pnpm_install_config`；CLI 显式传入 `dangerouslyAllowAllBuilds=true`、`production=false`，并优先使用离线缓存 |
| `build-agent-package` | [handlers/computer/packages.rs](../src/handlers/computer/packages.rs) 的 `build_agent_package` | 调用 `ensure_pnpm_install_config`；安装使用默认 `InstallOptions`，构建脚本策略取决于实际配置，不另加 CLI allow-all |
| 普通项目 dev server | [service/dev_server/start/launch.rs](../src/service/dev_server/start/launch.rs) | `write_npmrc` 创建或维护 `.npmrc` 并清理指定冲突键；不新增 allow-all，使用 `InstallOptions::prefer_offline()` |
| 普通网页项目 build | [handlers/build/build_exec.rs](../src/handlers/build/build_exec.rs) 的 `build_project_impl` | 安装前不调用上述配置准备 helper，使用 `InstallOptions::prefer_offline()`，读取项目实际配置 |

开发服务的 `DEV_SERVER_OVERRIDE_CMD` 分支不走表中的普通 package 安装分支。UserApp 多服务构建由 manifest 驱动，项目命令可能自行调用包管理器，不能据此表推断所有 UserApp 安装都经过同一审批处理。

`dangerouslyAllowAllBuilds=true` 表示允许依赖构建脚本运行，属于实际执行权限。表中是当前入口差异；不能仅根据项目使用 Vite 或其他框架就推断依赖不需要安装脚本，也不能假定所有入口都已启用相同策略。

## 配置准备会修改哪些文件

[pnpm_config.rs](../src/service/pnpm_config.rs) 提供两个层次：

- `create_pnpm_npmrc`：已有 `.npmrc` 只维护 `package-import-method=copy`，以及环境提供的 `store-dir`，保留 registry、scope、auth、注释和其他键。store 目录优先读取 `npm_config_store_dir`，再读取 `PNPM_STORE_DIR`。文件不存在时生成缺省模板，包含 registry 与 peer 安装设置。
- `ensure_pnpm_install_config`：依次执行上述准备、清理指定构建脚本配置，再补充缺失的 `dangerously-allow-all-builds=true`、`production=false`、`confirm-modules-purge=false`。补充逻辑只检查赋值键是否存在，已有 `false` 等显式值不会被这一步改成 `true`。

清理覆盖以下项目文件中的指定键，不是清空全部 pnpm 配置：

| 文件 | 清理范围 |
|---|---|
| `package.json` | `pnpm` 对象中的 `neverBuiltDependencies`、`onlyBuiltDependencies`、`ignoredBuiltDependencies` |
| `pnpm-workspace.yaml` | 顶层同名键 |
| `.npmrc` | 对应的 `never-built-dependencies`、`only-built-dependencies`、`ignored-built-dependencies` 行 |

其他字段保留。JSON 被修改时会重新序列化；YAML 优先保留原格式，只有解析与目标语义核验通过的结果才写回，必要时采用结构化重写。无法解析的 JSON/YAML 保留原文件并记录诊断，不用猜测内容覆盖源码。

`ensure_pnpm_install_config` 的各步骤是尽力处理，失败记录 warning 后仍尝试安装；这不表示安装成功。dev server 的 `write_npmrc` 直接传播配置准备错误。最终结果要结合实际 pnpm 退出状态、依赖与后续服务行为判断。配置中可能含私有 registry 凭据，排障和公开报告不要复制完整文件。

## 非交互安装与日志

[CLI 后端](../src/service/pnpm/cli.rs) 使用 `stdin=null` 执行 pnpm，并为安装组装：

```text
pnpm --reporter=ndjson install --no-frozen-lockfile \
  --config.confirmModulesPurge=false
```

`--no-frozen-lockfile` 允许开发项目创建或更新 lockfile；`confirmModulesPurge=false` 避免模块目录调整依赖 TTY 确认。`prefer_offline` 会追加 `--prefer-offline`，调用方的 `extra_args` 放在默认参数之后。安装清除继承的 `CI`、`NPM_CONFIG_PRODUCTION`，设置 `NODE_ENV=development`。

后端捕获 stdout/stderr，解析 NDJSON 事件并保留错误码、诊断和安装摘要；dev/build 还会写主日志与本次临时日志。失败码由 [classify.rs](../src/service/pnpm/classify.rs) 集中归类，未知码保留原值并归为 `Unknown`。安装退出成功只证明本次 pnpm 安装命令成功，业务仍需完成实际构建、启动及就绪验证。

## 被忽略构建脚本的定向处理

CLI 后端只在安装失败且提取到准确机器码 `ERR_PNPM_IGNORED_BUILDS` 时进入以下流程：

1. 从失败信息提取 `Ignored build scripts:` 包清单；有安装主日志时，必要时读取其尾部补充提取。
2. 没有可解析包名时，保留原失败，不猜测审批对象。
3. 对提取出的包执行非交互 `pnpm approve-builds <包名...>`。
4. 直接重试安装一次，不递归重试或改为全量允许。

普通审批失败会记录 warning，再由这一次安装重试给出结果；取消或进程树清理未确认时直接返回，不启动下一阶段。没有机器错误码、只有相似警告文本时，也不会触发该处理。此流程依赖实际 pnpm 对命令、错误输出和包清单格式的支持，需要针对使用的二进制验证。

所有阶段通过同一命令所有权和取消上下文管理进程树。超时、取消和清理未确认分别返回错误；旧进程尚未确认退出时，不能通过另起一次安装绕开清理。

## 排障顺序

1. 确认失败来自表中的哪个入口，记录实际工作目录、Node/pnpm 版本与使用的镜像或二进制身份。
2. 查看原任务或命令日志，保留 pnpm 退出码、机器错误码及 `failure_kind`。配置准备 warning 与最终安装错误应分别定位。
3. 配置冲突时核对项目 `.npmrc`、`package.json`、`pnpm-workspace.yaml` 和调用方参数；确认是否调用过 sanitize、写入是否失败、是否已有显式值。不要把 helper 被调用当作所有配置已成功修改。
4. 遇到 `ERR_PNPM_IGNORED_BUILDS`，确认日志中是否有可解析包清单、是否执行过定向审批及唯一一次重试。没有足够输入时保留真实失败。
5. 认证、网络、引擎版本、磁盘、权限和 lockfile 问题按各自错误处理，不通过全量放行脚本或跳过脚本来掩盖。
6. 安装通过后运行项目真实构建或 dev 服务，并验证需要原生模块的实际功能。不要用包已下载或 HTTP 受理成功替代该验证。

## 升级验证要求

变更 pnpm 二进制、安装参数、审批配置或日志协议时，对同一目标环境验证：

- 四条入口各自的实际参数、配置准备与真实安装结果，包括已有 `node_modules`、缺失或过期 lockfile 的非 TTY 路径。
- 包含真实依赖构建脚本的项目：确认脚本执行情况、审批结果与模块实际可用性；同时覆盖显式配置值和冲突配置。
- `.npmrc` 的 registry/auth 等无关内容保留；合法 YAML 的非目标字段保留；损坏配置不被猜测改写。
- NDJSON 与失败文本的错误码提取、包名提取、无包清单时的失败保留，以及审批失败后的单次重试。
- 超时和取消下的进程树退出确认，清理未完成时不继续安装或启动业务。

配置和 CLI 组件检查分别位于 `pnpm_config.rs`、`service/pnpm/cli.rs` 与 `service/pnpm/classify.rs` 的测试模块。组件结果、真实 pnpm 安装、应用功能和镜像部署是不同证据，升级报告应分别记录实际版本、命令、退出码及未覆盖项。

## 参考

- [pnpm 构建设置](https://pnpm.io/settings/build)
- [pnpm approve-builds](https://pnpm.io/cli/approve-builds)

外部资料用于理解对应版本的配置；本仓当前调用行为以上述源码为准。
