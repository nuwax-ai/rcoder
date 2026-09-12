# 执行台账

状态与行为证据分开记录，未有证据不得标记通过。

- [x] 基线工作区干净，HEAD 81b8a48；AGENTS约束已读取。
- [x] Spec/Plan/Task建立，Q01–Q12范围与分工固定。
- [ ] Q01/Q04/Q05/Q12：资源身份与删除、路由、唤醒、查询（runtime_review）。
- [ ] Q02/Q03：PG代次、条件操作、关停（core_review）。
- [x] Q06/Q07/Q11：Agent队列、SSE、安装持久化；247项组件测试通过，最终全量门禁另记。
- [x] Q08/Q09/Q10：主SSE、static、dev事件；组件与app-cli100项通过，Docker验收另记。
- [ ] test-e2e固定断言目录、新套件、隔离PG、失败清理证据。
- [ ] 全部门禁与app-cli版本冻结。
- [ ] app-runtime → dev-restart → dev-hot，核验实际产物。
- [ ] 严格userApp及Compose完整验收、定向清理。
- [ ] 提交、app-cli tag与六包npm发布核验。

本轮所有验证命令/结果和环境限制追加于此；前次审查通过不计本次修复验收。

## 本轮已取得的组件证据

- Q06/Q07/Q11：`cargo test -p agent_runner --lib`：247 passed / 0 failed / 0 ignored；日志 `/tmp/q06-q07-q11-green.log`。Q06 入队、慢订阅及 Q11 多 writer 基线红灯见 `/tmp/q06-red.log`、`/tmp/q06-subscriber-red.log`、`/tmp/q11-red.log`。Q07 有实际 SSE body 的取消、EOF、未 poll Drop、回放与终态测试；审查阶段已有确定性反例。
- Q08：基线 `full_subscriber_does_not_block_forwarder` 在100ms截止失败（`/tmp/q08-red.log`）；修复后 `cargo test -p rcoder --lib grpc::sse_stream::tests`：9 passed（`/tmp/q08-green.log`）。
- Q09：实际 HTTP A→B 基线仍返回 A（`/tmp/q09-red.log`）；扩展用例覆盖同端口换目录、端口变化、占用失败保持旧版本、恢复、异常退出重启及移除监听，3项 static_hosting 测试通过（`/tmp/q09-expanded-green.log`）。最终全量 app-cli 门禁另记。
- 严格报告：新增未登记场景测试先失败（`/tmp/strict-catalog-red.log`），登记目录固定于 `tests-e2e/tools/acceptance_steps.json`，来源为既有通过运行的断言集合，运行时不从实现推导。新增行为单独人工登记。
- dev-restart：真实 HEAD Make recipe 在隔离 stub Docker 环境下，down/up 两种失败均被末尾成功输出掩盖（`/tmp/dev-restart-baseline-proof.log`）；修改后工具集12项通过（`/tmp/e2e-tools-green.log`）。stub仅用于shell错误传播，不替代AI或Docker链路验收。
- 构建前四份 agent 启动脚本与 sibling build-agent-docker 源头逐字相同，当前无需同步覆盖。

以上均不是镜像或完整 E2E 验收通过证明；后续代码变更仍需最终冻结门禁。
- Q10：`cargo test -p file-server-userapp --lib start_events`：3 passed / 0 failed（`/tmp/q10-green.log`）。编排事件延迟、缺失Done、取消消费者的确定性测试已通过；真实Docker已运行+构建失败场景仍待执行。
- app-cli：修复后独立workspace Clippy通过、全量100项测试通过（`/tmp/app-cli-clippy.log`、`/tmp/app-cli-tests.log`）；npm查询当前0.3.3，Cargo.toml及独立Cargo.lock已改0.3.4，冻结后仍需重新验证版本与镜像。
- Q02/Q03：真实PostgreSQL17的7个lifecycle_contract用例通过（`/tmp/rcoder-q02q03-green.log`）：旧删除与墓碑、会话复用、容器换绑、load/sync身份、旧schema回填、并发失败关停、取消降级及超时后唯一drain。旧代次返回Superseded也有断言。真实PG全量存储回归、最终strict独立Compose执行另记。
- 报告身份复核：完整必经步骤也不能冒充另一个scenario/backend（红灯 `/tmp/report-identity-red.log`）。报告每行固定run_id/case_id/test_name，别名白名单在 `report_identities.json`；validator核对首条begin、唯一身份与末条terminal。新增Docker生命周期套件登记后共62个canonical报告身份。
- 取消工具：外层等待整个测试进程组（含清理子进程），PG使用命名卷及创建前ownership凭据；兜底以同Compose项目down -v清理，拒绝对PG裸rm。Docker生命周期测试也预登记容器/卷与run/case标签，兜底清理卷前校验归属。当前工具测试17项通过（`/tmp/e2e-tools-green.log`）。

## 最终复核增补

- PG独立完整回归：76 passed / 0 failed / 0 ignored（`/tmp/rcoder-q02q03-all-tests.log`）；本次专用PG容器及匿名卷已定向清理。
- 严格工具取消边界：收束与清理阶段延迟SIGINT/SIGTERM，结束后返回130；20项工具测试通过（`/tmp/rcoder-e2e-cancel-green.log`）。终端广播信号中断Docker CLI仍会明确计为清理失败，不算通过。
- Q04有旧代码确定性失败证据（历史文件名`/tmp/rcoder-q05-red.log`实际是watch晚订阅）；Q05/Q12的初次基线测试受编译错误阻塞，不把编译错误当作行为红灯。修复后断言及最终门禁结果另列。
- app-cli失败恢复意图与失败信息改为单次状态快照发布，避免中途Failed误触发平台释放操作保护；恢复结束前仍拒绝新部署。

- 本轮Workspace首轮发现日志测试提前读取文件：首条EVT回调不代表后一普通行已落盘（`/tmp/rcoder-workspace-log-eof-red.log`）。改为关闭输入并等待消费任务EOF完成，保留两类日志断言；不增加重试。
- macOS慢初始化已采样确认：系统代理发现进入CFBundle目录扫描，其他客户端等待同一系统once（`/tmp/rcoder-agent-runner-download-sample.txt:278`），Agent247最终通过，并非OnceCell永久死锁。冷部署Error路径改为不创建HTTP客户端；有IP才按需构建内部直连客户端。
- 静态异常退出曾复现同进程CLOSE_WAIT连接孤儿（`/tmp/app-cli-socket-diagnostic.log`）；单独SO_REUSEADDR不足，正在以连接任务所有权与drain屏障完成修复，不能以聚焦偶然通过替代全量验收。
- 准备锁close-only对fork/dup描述符无法立即释放：改显式unlock的RAII，保持blocking解压期间租约所有权；生产/builder Drop同理只释放OS锁，不清不确定操作的持久marker。

- app-cli冻结门禁：fmt --check通过、Clippy --all-targets -- -D warnings通过、默认并发全量108 passed / 0 failed / 0 ignored（`/tmp/appcli-owned-isolated-test.log`、`/tmp/appcli-owned-isolated-clippy.log`）。涵盖PreparationLease及六项停止/迁移静止性测试。
- Q09明确修复为自有连接JoinSet：监听先关闭，全部连接graceful/abort+join后才确认drain，异常monitor退出也触发回收。串行108过、并行排除8项进程fixture后的100项过，确定测试进程间fork描述符窗口干扰；静态生命周期用当前test exe独立进程隔离，30秒截止，保留全部真实HTTP/端口断言，默认全量仍并发。未使用REUSEPORT或重试。
- 协议3只在Unix声明进程组静止保证；非Unix保留协议2操作身份，不承诺子孙进程已静止。迁移停止确认不逆向执行数据库迁移。

- 首轮最终门禁：Workspace fmt、默认/Kubernetes/PG Clippy及全量workspace测试通过；K8s app_manager 116 passed，docker_manager 122 passed / 5环境门控ignored（日志 `/tmp/rcoder-final-*.log`）。此后归属复核增补仍需冻结门禁，不能用首轮结果覆盖新改动。
- 归属交叉核对修正生产Docker真实`app-id`标签；builder改为直接inspect真实service-type/identifier，禁止把请求回填类型当归属证据。补错族、错标识、缺标签、空ID回归。K8s PVC历史`service_type`键与真实创建一致，未盲目统一。
- 移除误贴在agent PVC ensure的删除禁令，destroy禁令保留；增加GET404→POST201正向API契约。
- Q05增补：Docker patch在镜像/配置准备成功后才按捕获物理ID删除，准备失败使用明确共享错误契约；仅Docker该已知未变更失败释放操作标记。后期create/start结果不确定仍保留标记，不声称旧容器可恢复。full_chain登记真实Pingora内容、容器ID/镜像ID/Running不变及后续hot重新取得锁断言。
- 严格工具最新22项通过（`/tmp/e2e-tools-current.log`）；npm实施前再次查询最新0.3.3，目标patch仍0.3.4。

- 冻结门禁：app-cli fmt及Clippy --locked -- -D warnings通过，108 passed / 0 failed / 0 ignored（`/tmp/app-cli-freeze-*.log`）；Workspace默认/Kubernetes/PG三组Clippy通过（`/tmp/rcoder-freeze-*-clippy.log`及`/tmp/rcoder-freeze-clippy.log`）。全量测试与完整E2E另记。
- 独立真实Docker预回归：冻结test binary在首轮app-runtime不可变镜像32572d37…上执行旧receipt/新容器保留、卷marker及失败更新真实HTTP断言，1 passed / 0 failed / 0 ignored，93.11s；`/tmp/q01-preflight-6ea24df6c16d4f8286cbbc22fa4ed7e5/test.log`，定向fallback cleanup结果`[]`。镜像内只使用shell/Python，此结果不替代最终Compose镜像与严格E2E验收。

- 冻结最终Cargo门禁全部通过（命令串退出0）：Workspace全量1884 passed / 0 failed / 35 ignored（普通环境门控入口，不能替代严格E2E）；K8s app_manager116 passed，docker_manager124 passed / 5 ignored，包括agent PVC ensure与builder真实标签新增回归。日志`/tmp/rcoder-freeze-workspace-tests.log`、`/tmp/rcoder-freeze-k8s-tests.log`。

- 用户追加注册表读侧优化：Mutex<HashMap>改为ArcSwap不可变快照，保留writer/file锁串行提交；注册表25项测试通过（`/tmp/registry-arcswap-tests.log`），新增保留真实读guard期间提交仍完成、旧快照不变且磁盘/新视图均更新的回归。后续Agent全量与Clippy另记；先前Workspace门禁早于此单模块追加变更。

- ArcSwap追加优化验收：agent_runner全量248 passed / 0 failed / 0 ignored（`/tmp/registry-arcswap-agent-tests.log`），Clippy -p agent_runner --all-targets -- -D warnings通过（`/tmp/registry-arcswap-clippy.log`），workspace格式检查通过。只修改注册表内存快照机制，未增加依赖，保留持久化提交锁。
- Linux builtin预回归确认静态失败制品原缺logs/env必填字段，准备阶段正确拒绝却被测试误等恢复；修正有效故障制品后最终runtime471ff154…上64条断言与清理全通过。supervisord预回归继续暴露已解析SPAWN_ERROR被误包为ShutdownUnconfirmed，已取得真实Docker红灯及Unix socket确定性红灯，仍在修复，不能计为完成。

- supervisord启动失败分类已修：已解析完整startProcess fault保留普通错误，必须经过server既有stop_all及动态组清空确认后恢复；stop/未知响应仍ShutdownUnconfirmed。真实Unix socket回归旧实现1fail（`/tmp/app-cli-rpc-fault-red.log`），新实现1pass（`/tmp/app-cli-rpc-fault-green.log`）。app-cli最终109 passed / 0 failed / 0 ignored、fmt及严格Clippy全通过（`/tmp/app-cli-rpc-final-*.log`）。正在重建runtime，真实supervisord修复后验收尚待执行。
- E2E fixture补齐必填logs/env；Docker错误/超时消息新增脱敏工具回归，Python工具23项通过（`/tmp/rcoder-e2e-tools-static-recovery.log`）。

- ArcSwap后当前Workspace fmt、Clippy及全量测试命令退出0：(1885, 0, 35)（passed/failed/ignored），日志`/tmp/rcoder-current-*.log`。普通Workspace环境门控不替代严格E2E。
- 最终app-runtime构建成功，镜像`sha256:6791815c576609e3d96e7ff61eefc5292c307b8a79dd33a488066c47fc26693d`，包含app-cli0.3.4及XMLRPC故障分类修复。独立工具链核验Python3.13 ABI、aarch64、JDK25一致。
- 首次dev-restart构建上下文误包含运行数据（未进入down/up），已仅取消本次构建。新增docker/userapp-workspace与docker/app-workspace排除；真实Docker合成上下文旧规则准确2项失败，新规则5/5通过，测试资源已清理，证据`/tmp/context-proof-44a9e685223f4da2af1ca8d9a9991057/`。工具25项通过。builder探测超时提示改为区分核验失败与真实版本不匹配；独立复测通过。

- 并行dev-restart再次触发工具链容器start超时，独立同一检查通过；修正注释与实际入口漂移为agent→master串行。真实make -j4命令级旧实现3项失败，新实现及全工具28项通过（`/tmp/rcoder-docker-build-order-{red,green}.log`），未放宽超时或增加重试。旧入口主镜像仍继续构建缓存，最终新入口重跑结果另记。既有app-cli-smoke容器ID/镜像ID/挂载集合再次核对一致。
