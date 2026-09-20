# 数据库迁移完成度复核（2026-09-20）

基线：`3ce5694a`。本文件记录源码和既有证据的复核，不把尚未运行的新测试算作通过。

## 主体实现与已有证据

UserApp、Preview、ProjectStore、Activity 已采用内部 Toasty 基础设施，业务 trait 不暴露 ORM；SQLx 已移除，初始化 SQL 收敛为四份。根默认与全 features、受影响组件、真实 PG、Turso、三平台原生核心流程分别见 [verification.md](verification.md)。这些证据不能替代完整 Compose、K8s 与原生异常矩阵。

S12 部分索引已有 PG 30 万历史规模及 Turso 实际查询计划证据。真实 PG COMMIT 确认丢失、选主连接关闭/取消/断连/响应超时均有通过记录。历史清单未勾选不能直接解释为这些实现不存在。

## 仍需补齐的原计划反例

| 项目 | 缺口 | 完成标准 |
|---|---|---|
| T0 取消完整受理 | 通用 Probe owner 测试并非 UserApp 完整受理，网络 ACK 丢失也不等于取消 future | 真实 admit 已进入事务后取消调用方，PG/Turso 的 operation/request/input/slot 完整保留，同请求重放原 ID；正在补测试 |
| S10 各关键写点回滚 | 现有本地测试主要在 admit 返回后、commit 前失败 | operation/request/input/slot 各写点后故障均整体回滚，原请求可重试 |
| S11 重建中途失败 | 正常 recreate 与旧身份拒绝不能证明 slots 删除到重建之间的回滚 | 在真实事务中间失败，旧 lifecycle、slots、历史保留；正常重试成功 |
| T0 PG DDL 中途失败 | 并发初始化及删除索引后的拒绝不能证明部分 DDL 回滚 | 独立 PG fixture 执行部分 DDL 后失败，本组件表及账本共同回滚，再初始化成功 |
| T0 PG checksum/未来版本 | Turso 已有专门测试，不能直接替代 PG | 独立 PG fixture 篡改后初始化拒绝，不改写原账本 |
| T0 类型与环境矩阵 | 证据分散，未形成完整逐项对应表 | 补 JSON/时间/整数边界、MSRV/最终依赖图及后端专用代码清单的证据映射 |

这些属于原 `tasks.md` / `schema-design.md` 范围。复核尚未发现新的确定性生产逻辑错误；应先补最小反例，失败时再修实现，不无依据重复全量构建。

## 部署验收进展

新主服务已使用包含 S12 的二进制并健康启动。`make dev-hot`、`make docker-build-agent-runner`、`make docker-build-app-runtime` 均退出0；builder/runtime 的 Python ABI 同为 `cpython-313-aarch64-linux-gnu`，Java 同为25，工具核验退出0。日志 `/tmp/rcoder-handoff-compose-build.log`、`/tmp/rcoder-handoff-toolchains.json`。

新的 Compose 发布链回归已启动，结果待定，日志 `/tmp/rcoder-handoff-compose-focused.log`。独立 SIGTERM/旧 keep-alive 夹具并行验证，不以正常退出码单独证明排空或持久保护。个人 K8s、完整原生矩阵及发布仍未完成，未 push。

## 随后补验结果

- T0完整UserApp受理取消：PG/Turso各1/1通过，真实四表及同请求重放均核验；仅测试门闩，无生产取消行为变更。
- PG迁移中途DDL失败、checksum/未来版本拒绝：独立PG17合并反例1/1通过，账本不改写、部分DDL全回滚及后续重试均断言。
- CR06真实SIGTERM：15/15通过，包含重启前持久保护与独占目录锁观测。
- S10/S11细分写点故障仍待补齐；完整Compose/K8s及原生异常矩阵未完成。

实际日志和证据路径追加于verification.md，不抹除上面的历史缺口判断。

随后S10/S11已完成：PG/Turso各9个受理/重建故障点通过；存储最终159/159、全features及PG-only严格Clippy通过。具体日志见verification.md。此前发布链因新测试helper遗漏默认代理地址而中断，已修复并启动完整Compose新快照；没有将中断轮记为通过。
