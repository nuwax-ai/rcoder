# Verification：原生运行时

## 2026-09-17 文档与源码核验

- RCoder：`feature-userapp` / `19dfd381`；开始时只有此前本轮审查目录未跟踪，无待审的未提交业务代码。
- 配套镜像仓库：`a4a4522`，本轮读取时工作树干净。镜像问题沿用 [B01–B05 证据](../development-review-2026-09-17/build-agent-docker-review.md)，未重新构建或部署。
- 实际完成：读取 app-cli、file-server-proxy、内嵌 file-server、npm 启动/下载器及发布矩阵，核对 N01–N10 的源文件定位；查阅官方 Node/Electron 文档确认必要调用边界。
- 产出：新增 Spec/Plan/Tasks、关联既有所有权文档、更新统一开发提示词。用户已明确 Electron 仅为使用场景，未将客户端开发纳入任务。
- 文档检查：`git diff --check` 退出码 0；本轮 12 个相关文档的本地链接、行尾空白及个人连接信息模式检查退出码 0，共 38 个本地链接有效（包含未跟踪的新文档）。此检查仅验证文档，不证明业务逻辑通过。
- 本轮没有修改 Rust/JS/构建配置，没有运行 Cargo/npm 业务测试，没有 SSH 登录测试机器，没有运行 Compose/K8s，没有 commit/push/tag/发布。
- 本文档不构成“三平台完成”或“无需依赖已验收”的证据。Tasks 全部待办；实现时追加新记录，保留本节历史边界。

## 开发验证记录模板

| 任务/反例 | 源码与产物身份 | OS/arch/执行模式 | 实际命令与退出码 | 结果与证据 | 剩余问题 |
|---|---|---|---|---|---|
| 待执行 | 待填 | 待填 | 待填 | 待填 | 待填 |

记录原生包内容及哈希、实际绑定地址、组件/操作身份，但不记录个人机器的账号密码、token 或其他秘密。正常退出、强制停止、未确认清理分别记录；环境失败不能写成逻辑通过。

## 2026-09-18 轮：R/B 合并修复中的原生增量

| 任务/反例 | 源码与产物身份 | OS/arch/执行模式 | 实际命令与退出码 | 结果与证据 | 剩余问题 |
|---|---|---|---|---|---|
| N01（ND04 部分） | rcoder `1fcc3860` | macOS ARM64 单元测试 | cargo nextest app-cli（supervisor::tests::pg_wait_is_gated_on_declared_need） | 声明式判定：migrate 命令/显式 APP_CLI_REQUIRE_PG=1 才探测；纯静态项目跳过 60s 轮询 | PG 完整运行计划预检（N 计划的"可完成前置校验"全项）未做 |
| R09 锁域（ND02 部分） | 同上 + runtime-state-layout crate | macOS（含 symlink/私有路径前缀） | crate 5 测试 + file-server token 契约测试 + app-cli 207 | 兄弟项目独立根/source-.run 同根/symlink 折叠/损坏 fail-fast | Windows junction、跨用户场景真机验证 |
| NT08 孙进程收束（app-cli 侧） | `2574f8c5` | macOS 真实进程 | tests/tree_lifecycle 2 反例 | TERM 忽略孙进程持端口：停机/失败兜底两路径端口释放 + 二轮可启动 | Windows/Linux 真机；proxy（file-server 侧子进程）未动 |
| NT12 终态/取消/SSE | `7e74cd0b`/`578ac9f6` | macOS 单元+mock | file-server 406→412 | Cancelled 不当成功、事件游标重放、Failed 带 error、orchestration 桥 | 部署 >10s 真链路时长覆盖随 Compose |
| ND01 反例对应表 | specs/development-review-2026-09-17/review-status.md | — | 文档 | R/B/N 合并修复映射建立 | R02/R03、N02–N10 大部、B05 未动 |

**未验证/未实施清单**（不以部分通过宣称整体）：N02 路径白名单重构、N03 端口计划、N04 Pingap 随包、N05/N06/N07/N10 proxy 原生形态、N08 file-server 侧硬编码 sh/ps/taskkill、N09 分发安全、ND05–ND12 对应项、NT01–NT16 三平台原生矩阵。

## 2026-09-19 追加：Compose config_hash 缺陷闭环（转引）

N04"随包版本锁定 Pingap"的一个同步点漏改（dev agent-runner 镜像 pin 0.14.1 vs app-cli 默认 0.14.3）导致 dev Compose 全部含代理配置的 dev/start 恒定 config_hash 确认失败。根因链、修复与验证证据完整记录于 [development-review-2026-09-17/review-status.md 第八批](../development-review-2026-09-17/review-status.md)。该缺陷同时是 Turso 轮 Compose 8 失败中 6 例的确认根因（另 2 例为其级联与 M4 锁信封）。NT 矩阵完整三平台实机、N07 认证层、R02 attach 语义仍未完成，状态见同文件。
