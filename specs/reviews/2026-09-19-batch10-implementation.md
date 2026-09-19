# 第十批实施记录：CR 修复、Toasty 前置验证与凭据链边界

## 1. 交付状态

**本轮没有完成合并方案。数据库全量迁移和 CR10 仍未实施，不能交给集成测试后直接发布。**

- RCoder 起点：`541a93a6073974b51cad5d405fe94d0e70896538`，改动未提交、未 push。
- 已实现 CR01–CR09 对应的第一批修复，组件验证结果见下文；完整场景验收仍未完成。
- Toasty 0.10.0 / Turso 0.7.2 已做独立真实数据库探针；**T0 全部门槛尚未通过，生产 Cargo 依赖及存储表结构没有切换**。
- CR10 的配置表、保存/状态 API、启动时捕获及应用版本、Secret/容器换代、双编排引擎凭据传递、journal 恢复、wake 错误分类、Java 接入说明均仍待实现。没有将现有 reset-password 偷换为“只保存”。
- userapp-workspace-template 修改了连接串编码；这只解决特殊字符一致性，不代表版本化凭据流程已经完成。
- build-agent-docker 只核查相关入口，未修改镜像/Helm 配置。原有未跟踪 `AGENTS.md` 保留。
- 未连接故障现场，未重置应用密码或清理任何既有数据库，未运行 Compose、remote K8s 或三平台完整 E2E，未发布 npm 或镜像。

## 2. 已修改的行为

### CR01 / CR04：排队执行身份和敏感输入

`runtime_kernel.rs` 在同一 admission 临界区内提升待执行操作，确认记录状态后安装 active 身份，保留本进程中的原始执行输入。脱敏后的磁盘记录不再被误作下一次执行的完整凭据。

`server.rs` 将 ID 带入 DeployRequest 私有字段，在消费动作时交接身份；dispatch 不覆盖旧执行者。Source / Artifact 队列均有内核反例。

### CR02 / CR03：Stop 和部分受理失败

Stop 不再进入启动排队槽。旧执行失败/取消/未知不提前终态化 Stop。覆盖旧排队记录失败后，新 Accepted 转入恢复保护，重放仍返回同一操作。

补充沿执行链发现的两处问题：

1. startup 的 ready channel 提前关闭时，必须先收束 supervisor 的真实结果，不能抢先读取排队控制信号，把旧启动失败当成新请求的停止未知。
2. Running 已提交并清空当前执行 ID 后，停止旧服务失败必须按所消费的 Stop/Restart ID 记录 RecoveryRequired。builtin 对尚未执行就取消的 Source 请求继续等待旧 supervisor，不停止或丢弃其句柄。

### CR05：Turso worker 的关闭确认

关闭入口使用 watch 共享最终结果。发送 shutdown 信号失败也必须 join worker；首个 waiter 被取消不取消 join；正常及异常结束都通知后续和并发 waiter。reply 通道消失表述为“结果未知”。

### CR06：关机受理和生产者收束

新增共享 OperationFlightGate，关闭和受理通过原子状态互斥。spawn 前获得 guard；builder 外层观察者与内层执行共同持有它。AppService 的物理操作 guard 与 HTTP 受理使用同一 gate。

HTTP 停止 accept 后 drain 已接收连接；已有 keep-alive 不再继续受理。Pingora、内嵌 file-server-proxy、后台生产者、恢复扫描和业务操作收束后才关闭存储。统一 65 秒 deadline，超时明确返回错误，不再继续调用 storage.shutdown。main 提前订阅关闭信号，初始化期间收到信号后重发给晚创建的任务，避免丢失 SIGTERM。

PG leader supervisor 保留并等待代际任务，选举器提供可等待的关闭结果；新代际不与本进程尚未退出的旧代际并行。恢复扫描器不再用独立 30 秒预算提前返回“已排空”，由总关闭预算统一控制。

组件测试验证了真实 TCP 请求仍在 handler 内时不能提前结束 HTTP server，以及关闭后的旧 keep-alive 失效。真实 SIGTERM / PG 网络故障 / K8s 验收仍待执行。

### CR07：Dev 删除复用实际租约

AppOperationGuard 将实际 builder lease 借给删除器，借用方不能释放物理租约；外层在 durable 终态之后唯一释放。Dev Docker/K8s 都走相同 runtime lease 入口，避免第二次 acquire 及回执 token 变更。

`capture_with_lease` 是有类型的所有权通道，不使用 receipt 代替授权。不支持的适配器明确拒绝。借用路径不反向等待 builder 的本地锁，避免与 ensure 的“本地锁→物理租约”形成锁序反转。

组件覆盖了释放责任和现有 storage destroy 链，**真实双副本争锁、捕获/删除/终态写失败后的恢复矩阵仍需补验**；不得据此宣布 K8s CR07 验收完成。

### CR08：代理关闭

代理持有连接 JoinSet，stop 的并发调用等待同一个结果，取消首个 waiter 不释放实例锁。连接 drain 结束后才释放锁。超过 10 秒强制终止并 join 残留连接，返回明确错误，不能报告正常 drain 成功。

默认 public bind 保持允许；没有增加强制 token/loopback 前置。

### CR09：事件游标

每个内核通过唯一事件写锁分配持久序号，accepted/progress/terminal 共用入口；终态幂等。去掉根据 `after_seq=MAX` 或旧条数推算序号的逻辑；事件写失败返回 false 并记录错误，不虚报已落盘。

终态事件收口到终态记录写入入口，排队替代及部分受理失败也会发终态。新增 progress 后终态续读、排队替代终态续读、并发 16 次追加唯一序号、写失败布尔结果反例。完整 HTTP/SSE 重连验收仍需补充。

## 3. Toasty T0：真实发现及实施影响

可复用探针在 `tools/toasty-probe/`，独立 workspace，包含精确 lockfile；不进入 RCoder 发布依赖。

真实临时 Turso 文件和本轮创建的独立 PG16 容器验证了模型读写、FK/CHECK 拒绝非法写、CAS 返回 1/0、rollback 无残留、连接对象收束。PG 还验证了独占 advisory lock 的实际竞争。

### 需要修订/补齐的基础设施

1. 官方 PG Connection::connect 内部 spawn transport，不暴露 JoinHandle。仅计数外层 Connection/Driver 的 Drop 不能证明该 task 已结束。公开 `Connection::new(client)` 可以接入自行持有的 transport，探针已验证这条路径；但生产 TLS/URL 选项、超时、连接创建取消、失败共享结果仍需完成，不能直接复用探针的 NoTls 工厂。
2. Toasty 0.10.0 对 PostgreSQL raw query 返回 `void` 的自动推断会 panic。advisory_lock 用不解码返回值的 `sql::statement`；try_advisory_lock 仍读取 bool，不能通过忽略数据库结果决定获主。
3. 首轮“drop 后立即一次 try_lock 必须成功”断言过强。即使本地 transport 已退出，PG 也可能稍后处理断连。探针现在有界观察 PG 实际锁结果；这不等于靠等待固定时长授权解锁，也不能把首轮失败称作已证明的永久锁泄漏。

**尚缺**：完整新 schema、双 store 并发 CAS、业务事务取消、BEGIN/COMMIT/ROLLBACK 故障注入及污染连接隔离、迁移并发/checksum/未来版本、Turso runtime 和目录锁完整关闭、生产 TLS。T0 tasks 不应整体打勾。

## 4. 模板与构建仓库

模板：Go 使用标准 URL 编码，保留 IPv6 和 Unix socket host；Python 用 SQLAlchemy URL 生成一致的用户名/密码；Rust 的数据库名也编码；Drizzle Kit 与 Next runtime/migrate 共用 buildConnectionString。

构建仓库仍需配合 CR10 处理：

- app-runtime-base 的 pg-supervisor-entry.sh 仍把临时 PG 启动、createdb、停止失败用 `|| true` 忽略；PG_VERSION 已写入但业务库未创建时，重启不补建。
- agent-runner 的入口虽会后台补建，但将所有 createdb 错误当“已存在”，不能作为数据库初始化成功证据。
- 两边都不能直接把全局 POSTGRES_USER 改成业务账号：已有 PGDATA 的管理员角色需要稳定管理身份。应与管理凭据/业务凭据分离一起实施，并同步 RCoder docker/ 下的副本。
- 本轮没有改变上述启动脚本，避免仅改默认账号或建库逻辑却绕过版本化凭据流程。

## 5. 实际验证

以下是组件证据，环境门控 skip 不代表真实集成已通过。

| 命令/范围 | 结果 |
|---|---|
| 根 workspace `cargo nextest run --workspace --all-features --no-fail-fast` | exit 0；2382 passed / 13 skipped |
| 受影响 5 crates 全 features | exit 0；941 passed / 1 skipped |
| app-cli 独立全 features（最终轮） | exit 0；230 passed / 0 skipped |
| 根受影响 crates 默认与 all-features Clippy（all-targets） | 两档 exit 0 |
| 受影响 5 crates 默认 features | exit 0；919 passed / 0 skipped |
| Go `go test ./internal/config` | exit 0，IPv6/TCP/Unix socket 与特殊字符反例通过 |
| Python 隔离依赖 `unittest discover -s tests` | exit 0，1 test |
| 模板 `pack-templates.mjs` / `test-local.mjs` | exit 0；生成器 39 passed |
| Toasty 独立探针，真实 Turso / PG16 | 各 exit 0；范围仅第 3 节 |

首轮失败没有被藏掉：root fixture 未给 image 导致环境依赖，现显式给固定测试镜像，保留回执断言；app-cli 新 Deploy 夹具错用了 Source，已纠正；失败重试用例暴露上述真实竞态，代码修复；进程树夹具的阻塞日志读取未受 timeout 约束，改为有界通道读取并固定日志过滤。最终新增 Stop 反例初次用了错误 expected_revision=1，而启动不推进该 revision，已改为实际 revision=0。

完整命令日志暂存 `/tmp/rcoder-batch10-*.log`。本报告不改写历史第九批验收结论。

## 6. 后续必须继续的开发，不只是跑 E2E

1. 完成 T0 的生产连接关闭/取消/故障隔离设计和缺失反例，再决定进入批量迁移。
2. 实施 Toasty 基础设施、统一 UserApp/Preview/Project/Activity 存储、新表约束与四份初始化 SQL，删除全部 SQLx 和旧 direct Turso。
3. 实施 CR10 全链：pending/applied/config revision/操作绑定 → 保存和查询 API → 显式启动捕获确定版本 → 管理通道/PG TCP 验证 → 冷换代与同版本迁移/业务进程 → journal 及 wake 分类。流量唤醒仅恢复 applied；dev/prod 隔离。
4. 提供已实现接口的 Java 接入说明，并保留“Java 未联调”状态。不能现在宣布 Java 可以切换到一个尚不存在的保存 API。
5. 补 CR01–CR09 剩余执行/恢复集成反例，以及 CR10 的全部反例，再交给 Claude 运行完整 Compose、remote K8s、三平台和测试库切换。

### 最终日志索引与验证顺序

- `rcoder-batch10-all-features-nextest.log`：完整根 workspace，全 features；其后只补了恢复扫描器统一关机预算，由默认 features 组件及最终两档 Clippy 覆盖。
- `rcoder-batch10-default-nextest.log`：受影响 5 crates 默认 features，包含上述恢复扫描器修改。
- `rcoder-batch10-app-cli-complete.log`：最终 app-cli 全套 230 例，包含停止错误身份、排队替代终态与并发事件游标。
- `rcoder-batch10-clippy-default-final.log` / `rcoder-batch10-clippy-all-final.log`：根受影响组件两档静态检查。
- `rcoder-batch10-app-cli-clippy-complete.log`：最后事件收口后的 app-cli Clippy。
- `rcoder-batch10-toasty-{pg,turso}.log`：独立数据库基础探针；自己创建的临时 PG 容器已按任务标签核验并停止。
- 根与 app-cli 的 fmt --check、两仓库 git diff --check 通过。Rust 模板未单独编译；Next 未做应用构建。

这些日志位于 `/tmp/`，后续完整集成测试应另存源码摘要、镜像身份和部署报告；不能用本表替代。

最终静态检查均 exit 0；最后 main 信号订阅位置调整经 `cargo clippy -p rcoder --all-targets --all-features` 验证（日志 `rcoder-batch10-shutdown-main-clippy.log`）。本轮未另建基线 worktree 对全部新增反例逐个做修复前复跑；历史审查探针、这轮实际失败及修复后组件结果分开记账，不声称全部反例都有本轮 red/green 双跑证据。
