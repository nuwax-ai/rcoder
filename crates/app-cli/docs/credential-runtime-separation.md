# PG 凭据更新与业务生命周期分离

## 当前需求

PG 改密立即生效；业务何时重启由用户决定。改密本身不请求 app-cli 停服务、替换容器、交接运行代次或重新部署。

## app-cli 调整

删除仅用于凭据冷换代的能力：

- `seal-source` CLI（包括离线 source seal）。
- `/v1/runtime/configuration/source-seal`、`prepared`、`activate` 及 OpenAPI 注册。
- `ConfigurationGate`、generation handoff、source-sealed 专用受理屏障。
- journal 中 `generation_handoff` 和请求中 `requires_configuration_activation` 的新写入。

旧持久 JSON 中的这两个退役字段仍可读取，不改变原 operation、generation、活动制品和边界。旧 source-seal 文件不再参与运行授权；此次修改不会删除 journal 或修改其运行代次。

正常 `serve`、`run`、热部署及用户显式 Start/Restart 继续使用原有运行控制协议。`run_pg` 作为显式启动输入保留，仍不写入 journal；它不再建立“等待平台改密 ACK 才启动”的配置 gate。

## 保留的独立安全约束

- OwnerGuard、锁与管理 API 绑定顺序。
- 原 operation 身份、取消、未知结果与持久化失败保护。
- proxy 写操作的 auxiliary-writer 受理与取消未知保护。
- 已停止运行态在普通恢复时不复活、不预写 Switching。
- RestoredActive、活动制品与 source/.run 执行目录恢复。
- 迁移未确认保护、普通 journal generation 校验。身份或目录未知不会被改写成当前 owner 的普通失败回执。

## 回归范围

删除的是 source-seal/handoff/activation 专属协议测试；其依赖的通用安全断言保留或拆为独立用例。新增/调整：退役 CLI 拒绝且 run 仍可解析；三个退役 API 返回 404 且常规管理/部署 API 仍注册；退役 JSON 字段兼容；未知执行目录仍阻断；Stopped 普通恢复保持活动 journal 原样。

本次仅完成源码与格式核对，尚未运行 Cargo。集中验证应执行独立 app-cli 的 fmt、Clippy 和 nextest，不能用根 workspace 检查代替；还需验证平台 PG 更新没有发出停服务/换容器/自动重启请求。
