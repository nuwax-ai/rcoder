# PostgreSQL 身份契约升级与验证

## 行为与版本边界

0004_lifecycle_identity 为既有 project/session 一次性回填随机不透明代次，持久化 project、session、container 墓碑。普通更新保留项目代次；删除后的新生命周期使用新代次及明确 predecessor。代次不从时间戳派生。

旧代次删除、清会话和旧 upsert 重放不能影响新代次。清会话只处理受理时捕获的会话身份。物理容器清理另校验 container_id 和当前项目关联；资源清理专用路径同时校验项目代次及容器身份。SQL 通过条件写入的影响行数返回 Committed / Superseded；Superseded 是过期操作，不记为实际业务修改。事务日志分别记录两类计数。数据库超时/失败为 Deferred，原操作继续保留在重试队列。

同一个活跃项目代次内，整行更新仍沿用 last_activity 的已有排序规则；时间相等按 SQL 提交顺序处理。本轮代次屏障不等于同代次字段 revision CAS。

load、session 回源和跨副本 sync 保留数据库身份；sync 使用读取前内存快照检查本地并发修改，避免旧快照覆盖新本地记录。同步锁不跨数据库 await。每个 SQL 事务提前按排序后的键集合取得事务级 advisory lock，避免跨键锁顺序反转。

0005 的 UserApp 元数据迁移由同一发布提供，和 0004 一并通过 sqlx migration runner 顺序执行；该域同样要求条件身份删除。

## 部署顺序

1. 停止接收新写请求，等待旧副本的在途请求结束；保持数据库可用。
2. 让所有旧 writer 完成关停 flush。发生失败/超时必须先排查并确认未落盘操作，不能把进程退出视为落盘成功，也不能直接滚入新旧混跑。
3. 旧 writer 全部退出后，备份数据库，使用同一新版本执行 additive migrations 0004、0005。sqlx 的迁移锁负责迁移并发，不负责让旧业务 writer 理解新契约。
4. 仅启动带代次契约的新副本，验证 boot load 与 session 回源身份稳定，再恢复写流量。
5. 检查 Committed / Superseded / Deferred 日志和持久化 pending 状态。业务请求超时/取消时，已登记的操作仍由 RAII 转入重试队列。

**不支持旧 writer 与新 writer 混跑。** 旧二进制缺少身份前置条件，仍可能无条件删除新行。也不支持通过删除新增列/墓碑来直接降级旧代码。墓碑在仍可能出现旧操作重放期间必须保留；本轮没有自动回收墓碑。

## 关停结果

关停先关闭新写受理，等待已登记 durable 写完成或转入 fallback，再通知唯一 writer 排空。所有 flush 调用观察同一个 watch 最终结果。调用方等待超时返回 TimedOut，底层 drain 继续；后续调用读取同一最终 Complete / Incomplete，不能重启另一次 drain。失败计数、未落盘数量和任务异常都不能折叠成 Complete。

## 可重复验证

专属 PostgreSQL 17 容器/数据库由本次 E2E run 创建和登记：

```sh
RCODER_PG_TEST_DSN=<本次run专属DSN> RCODER_PG_TEST_STRICT=1 \
  cargo test -p rcoder-storage --features pg --lib lifecycle_contract -- --nocapture
```

严格模式缺少 DSN 直接失败；每个真实数据库用例检查 PostgreSQL 主版本。当前七个契约覆盖旧删除、新身份、墓碑、会话复用、容器换绑、load/sync、旧 schema 回填、并发失败关停、等待超时、取消转队列和唯一 drain。仓库普通测试仍允许未配置数据库时跳过，不能把这种跳过当成集成验收。

真实集群 K8s 多副本运行不是本轮证明层级；这里是隔离 PostgreSQL 的真实 SQL/存储组件契约测试。E2E runner 负责冻结测试二进制、逐个核验实际用例数量/结果、采集脱敏日志并只清理本 run 资源。
