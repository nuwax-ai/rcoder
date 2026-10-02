# app-cli 项目配置校验

`validate` 检查 UserApp 源码 workspace 的 manifest、服务依赖、端口分配和代理配置，不生成 lock、不构建、不启动服务。app-cli 0.3.13 提供此命令；实际安装是否支持，以 `app-cli validate --help` 为准，不能仅依据源码版本号推断目标环境已升级。

```bash
app-cli validate --workspace ./my-app
app-cli validate --workspace ./my-app --dev
app-cli validate --workspace ./my-app --json
```

显式传入 workspace。省略时沿用 `APP_CLI_WORKSPACE` 和既有默认目录，不默认使用当前目录。`--dev` 选择开发代理规则，不放宽生产 manifest 的必填项。

## 校验内容与边界

- workspace 与一级服务 manifest 的语法、字段、枚举和现有规范。
- 服务 ID/路由唯一性、依赖存在性和拓扑、启动探测与 bridge 声明。
- 与 gen-lock 相同的内部端口分配和代理规则。
- custom/extend 配置源的读取、解析、服务引用与平台护栏。

只扫描一级服务目录；没有 manifest 的 docs/packages/scripts 不会被当作服务。需核对报告中的服务与预期清单，防止漏接模块。源码解析/读取不完整时，服务摘要会标明不完整，跳过不能可靠判断的依赖阶段。

校验不读取旧 `release.lock.toml` 作为权威，不更新配置、lock、运行状态或日志。不检查依赖安装、构建脚本是否能执行、制品内容、端口是否被占用、业务接口、数据库连接或实际 owner 能力。缺少 dist/zip 或尚未安装依赖不作为配置失败。

代理检查保留上游 Pingap 的原生规则：普通 managed 路由使用内部地址，某些 custom 配置的显式 static hostname 会触发 DNS 解析。因此不是完全离线检查，也不是上游服务可达性探测。新 validate 的总体观察预算为 30 秒，原生代理校验预算为 10 秒；观察失败/超时返回 3，不宣称项目配置非法。原 gen-lock 和运行时入口不增加这个新预算。

## 输出与退出码

默认文本列出问题、文件、可用的字段/位置和修复建议，以及未执行/不覆盖的检查。

`--json` 的 stdout 是单一报告对象，包含：

| 字段 | 含义 |
|---|---|
| report_version | 报告版本，目前 1；不是 manifest schema 版本 |
| scope / profile | configuration；prod 或 dev |
| valid | 所请求的配置检查是否全部通过，不表示应用已运行 |
| topology_checked | 是否实际完成跨服务拓扑检查 |
| services_complete | 是否读取了完整的服务输入；false 时清单可能只有部分有效模块 |
| services | 服务 ID、目录、端口、路由和生效 strip 策略；没有 env/命令内容 |
| diagnostics | parse/validation/io/internal 类别，以及文件、可用字段、原因和建议 |
| skipped_checks | 因前置失败没有执行的检查 |
| not_checked | 本命令职责外的构建、产物、运行与外部依赖检查 |

报告不输出原始源码行、完整配置或 env/命令值；保留服务摘要、诊断所需的标识与路径，可用时保留解析行列。依赖转换器没有 span 的错误只能定位配置文件或目录，不能伪造具体位置。输出通道损坏时可能无法给出完整 JSON，进程仍以失败退出。

| 退出码 | 含义 |
|---|---|
| 0 | 配置检查通过 |
| 1 | 配置语法或语义不合法 |
| 2 | 命令参数错误 |
| 3 | 文件观察、DNS/超时、输出或内部失败，无法完成检查 |

混合问题中观察失败优先返回 3；JSON 仍保留已确认的配置问题，不将未读到的模块伪报为依赖不存在。

## 后续工作

配置通过后再执行实际构建与运行：

```bash
app-cli gen-lock --workspace ./my-app
app-cli build --workspace ./my-app --deploy-dir ./my-app/.local-deploy
```

gen-lock 会写派生 lock，失败时不能用旧 lock 声称本轮通过。按项目的真实 dev/prod 方式启动并验证页面、真实 JS/CSS、API 或 worker 任务；已有平台 owner 时优先使用平台控制接口。配置检查不成为 UserApp Start/Stop/Restart 的新增门禁。
