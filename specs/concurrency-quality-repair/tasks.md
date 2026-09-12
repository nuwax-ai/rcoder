# 执行台账

状态与行为证据分开记录，未有证据不得标记通过。

- [x] 基线工作区干净，HEAD 81b8a48；AGENTS约束已读取。
- [x] Spec/Plan/Task建立，Q01–Q12范围与分工固定。
- [x] Q01/Q04/Q05/Q12：资源身份与删除、路由、唤醒、查询（runtime_review）。
- [x] Q02/Q03：PG代次、条件操作、关停（core_review）。
- [x] Q06/Q07/Q11：Agent队列、SSE、安装持久化；247项组件测试通过，最终全量门禁另记。
- [x] Q08/Q09/Q10：主SSE、static、dev事件；组件与app-cli100项通过，Docker验收另记。
- [x] test-e2e固定断言目录、新套件、隔离PG、失败清理证据。
- [x] 全部门禁与app-cli版本冻结。
- [x] app-runtime → dev-restart → dev-hot，核验实际产物。
- [x] 严格userApp及Compose完整验收、定向清理。
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

- 当前共存源码门禁：workspace fmt、默认及Kubernetes Clippy通过，全量1892 passed / 0 failed / 35普通环境门控ignored；app-cli独立fmt、严格Clippy及109项测试通过（`/tmp/rcoder-coexist-*.log`、`/tmp/app-cli-coexist-*.log`）。包含并行提交中的/version改动，不能与先前纯修复计数混用。
- 串行 `make dev-restart` 返回0（`/tmp/rcoder-dev-restart-serial.log`），已完成agent-runner、主镜像构建及Compose重启；`make dev-hot`执行中，运行二进制身份和严格E2E尚待验证。
- 构建中转镜像旧完整编译层约1.91GB，导出/解包耗时406.9秒；已增加仅二进制的artifacts阶段。解析器检查全部镜像层，不将Docker注入的容器/dev文件误认为镜像内容。实际新阶段验证待完成，不提前声明性能收益。
- 真实PG严格预检run `894652ff51884541992eea53a31c23ea` 的Compose创建超时后出现迟到容器，原清理空库存不足以证明结束；仅按登记身份收束并保存cleanup-amendment，原失败不改为通过。新增pending创建状态与专用兜底，旧行为确定性红灯 `/tmp/rcoder-pg-late-create-red.log`；PG、hot、构建清理及镜像层工具共51项通过（`/tmp/rcoder-tools-final.log`）。完整严格PG与Docker验收仍待执行。

- 严格PG独立run `3b33ddcdb02f4d8188cf3479c911425c` 全部7项生命周期契约及清理通过，源码指纹前后一致。真实二进制artifacts打包通过，原create/cp路径、0755、SHA及全部镜像层检查均符合契约；证据 `/tmp/real-artifact-51e7f11384944382a5fa4298316565bb/result.json`，测试容器/镜像清理后库存为空。
- `make dev-hot` 首次完整成功，三份SHA（运行中/proc/1/exe、/app/bin、target-console产物）均为384473a7…且健康正常（`/tmp/rcoder-final-running-identities.json`）。随后发现缺口又修改主服务，此SHA不代表最终修复产物。
- 诊断严格run `4ec61e766f66436aaf118c9f95c3ed64`：22 pass、3真实失败；修复准备完成后主动SIGINT收束，另1个正在执行场景标记失败、7个未执行场景aborted。不是最终验收通过。两处实际问题：HTTP purge取消导致已完成builder删除仍遗留prod marker；物理删除漏清运行时缓存，归档重建复用已删除ID。保留全部原始报告，不把错误改为通过。
- purge取消：确定性旧实现成功收尾断言失败，unknown/panic保护断言通过；修复后3项全过、app_manager全lib119 passed（`/tmp/rcoder-purge-cancel-{red,green,full-green}.log`）。Docker缓存/创建/重建身份增补：旧实现4项明确红灯，最终6项新增回归全过、lib57 passed / 0 failed / 5原环境ignored，严格Clippy和fmt通过（`/tmp/delete-cache-rebuild-*.log`）。均已合入，主源码最终门禁及重新dev-hot执行中。
- 清理诊断补丁保持失败定义和60秒截止：Python56项、Rust3项通过（`/tmp/rcoder-cleanup-diagnostics-{python,rust}.log`），记录脱敏错误以区分传输超时与业务冲突。

- 合并后完整Workspace门禁：fmt、默认/Kubernetes Clippy、全量1904 passed / 0 failed / 35普通环境ignored（`/tmp/rcoder-post-e2e-*.log`）。随后根复核补充canonical定位的一处生产变更，针对性门禁另记，不把此前全量结果冒充新增用例的证据。
- 新主进程SHA三处一致3d246c00…（`/tmp/rcoder-repaired-running-hashes.txt`）。归档重建严格run `de98310a5396444482fc652f6f4ef904`通过，未调整原超时。OpenAI严格run `bd3f84393eaa43dc8e363585f424a13f`仍因60秒清理请求超时失败，但后台purge及随后幂等purge均正常完成、marker为空，取消永久冲突已消除；须在不并行构建条件下复核耗时，不把失败计为通过。
- 旧诊断run两个非空marker经原任务完成日志、物理容器不存在、新主进程重启及非阻塞文件锁/原UUID校验后定向收束，随后正式purge均返回0000。证据 `tests-e2e/reports/4ec61e766f66436aaf118c9f95c3ed64/operator-recovery.json`；原失败结果未改。本run的29个登记资源名均未发现容器/数据目录残留，既有应用保持。
- canonical定位新增确定性红灯：真实preflight→start_prepared访问project-physical而非pod-physical，`/tmp/rcoder-canonical-key-red.log`。已改用preflight派生标识，保留原重建触发边界，绿色回归及最新热编译进行中。
- 未合入的优化：仅按RCODER_URL关闭共享测试客户端代理会影响独立指定的远程LB/Pingora目标，因此拒绝候选 `/tmp/rcoder-loopback-proxy.patch`；不能以本地提速改变跨环境路由行为。

- canonical定位最终门禁：默认docker_manager 58 passed / 0 failed / 5原环境ignored，Kubernetes 131 passed / 0 failed / 5原环境ignored；fmt与workspace默认/Kubernetes Clippy通过（`/tmp/rcoder-canonical-*.log`）。最新make dev-hot返回0，健康通过，运行中与两处编译产物SHA均85cc02b6…（`/tmp/rcoder-canonical-running-hashes.txt`）。既有app-cli-smoke容器、镜像和挂载集合不变。

- 独占本轮构建结束后的OpenAI严格run `b61f73c5f34b484ba4f6f5441de77a64`通过，purge耗时12.131秒，未调整60秒截止；指纹一致。最终runtime双引擎严格run `e6d7ef7380e741b48681e8b8c3cbf3d0`通过，builtin及supervisord各64条断言均成功，包括XMLRPC启动失败恢复、静态目录/端口/服务移除及清理。准备执行最终完整两组套件。

- 补充只读审查发现旧stop及晚查询缓存回填窗口，诊断run `df2b560de9834ffab32fc34261b207e7`取得11 pass后主动取消，1活动场景fail、21aborted；保留processgroup PermissionError及诊断No such container，未改绿。只读证据确认16登记资源名/14物理ID均无容器及数据残留，marker为空；原及fallback purge均完成metadata收尾，无需手工恢复。
- 新增4个真实本地HTTP barrier回归旧实现全部断言失败（`/tmp/rcoder-cache-epoch-red.log`）；生产补丁已合入，另2个新token API测试独立证明失效入口边界，不冒充旧API红灯。绿色门禁进行中。

- 最终缓存门禁：默认64 passed/K8s137 passed，各5原环境ignored；fmt及workspace默认/K8s Clippy退出0（`/tmp/rcoder-cache-epoch-*.log`）。共存的其他任务将默认闲置阈值5天改24小时，三文件保持独立归属，最终dev-hot已包含共存源码，三份运行SHA均f62bd3df…且健康正常。
- 当前依赖重新构建app-runtime成功，最终镜像`sha256:1cbdc0ee13408cf8d2047c05fb6a905823b82f9650f94ce396022e580e174553`；此前6791815c镜像双引擎通过不能替代该镜像最终验收。
- 补跑workspace真实发现本机health可用而无E2E_REPORT_DIR的入口panic（`/tmp/rcoder-final-coexisting-workspace-tests.log`）。已统一前置上下文门控：17项lib通过；实际薄入口普通缺上下文0.030秒skip、显式strict缺上下文0.019秒exit101，另PG/Compose fault/K8s smoke均0.00秒skip且3个K8s ignored未执行（`/tmp/rcoder-e2e-context-*.log`）。该失败不算通过，修复后的全量workspace正在重新执行。

- 最终共存源码workspace全量命令退出0：1916 passed / 0 failed / 35原环境ignored，含全部文档测试（`/tmp/rcoder-final-workspace-green.log`）。E2E前置门控Clippy通过，Python工具56 passed（`/tmp/rcoder-e2e-context-clippy.log`、`/tmp/rcoder-final-tools-green.log`）。接下来冻结执行新runtime1cbdc0ee镜像与main f62bd3df二进制的完整严格验收。

- 完整严格 run `d0c7032b206d4df28a7e139b433e7659`：33项实际执行，31 passed / 2 failed，无跳过或中止，源码指纹一致。新runtime1cbdc0ee双引擎各64条断言及PG/Docker身份契约通过。失败分别是Q10测试未携带兼容入口X-App-Id导致故障manifest未安装，以及全链Java Maven下载失败；本轮不是最终通过。
- Q10 fixture补齐既有X-App-Id契约，安装失败立即进入原清理/失败出口，不继续用旧manifest测试；严格聚焦run `00ed9d2aab6e4e12984d83282aefd1e6`通过。构建或制品准备失败未发出生产创建请求，不再执行虚假的生产删除；创建结果不确定的原清理分支保留。两项测试补丁fmt及rcoder-e2e Clippy通过（`/tmp/rcoder-final-fixtures-*.log`）。
- Maven诊断同builder镜像/rcoder_default网络完整wget成功，9395475字节，SHA256为5af3b743dd8b876b5c45da33b676251e5f1687712644abb4ee519ca56e1d89ce，专用诊断容器清理成功（`/tmp/rcoder-maven-probe-aacdd2ca931049d1a1a897984c74aee1/`）；该结果排除永久失效URL，不解释原瞬态失败，也不替代完整部署回归。
- 用户另行提交`f96ce85`将默认闲置阈值最终改为2小时；保留该提交，早前24小时共存二进制不能声称包含此最终默认值。提交前发现stop inspect404漏退役旧缓存，正在补确定性回归；聚焦全链run `6de9259022cb43f2bc7ec2cd7510ba3c`在Cargo编译等待期间主动取消，未执行任何场景，不作为验收证据。
- stop404提交前回归：真实HTTP barrier的inspect404与DELETE404在旧实现均失败（`/tmp/rcoder-stop-404-red.log`，0 passed / 2 failed）；前者残留旧actor别名，后者返回Bollard404。统一为幂等物理退役，继续保留并发replacement，未恢复按project键无条件删除。修复后门禁及最终热编译另记。
- stop404修复门禁通过：docker_manager默认66 passed / 0 failed / 5原环境ignored，Kubernetes139 passed / 0 failed / 5原环境ignored；fmt、docker_manager/rcoder默认及Kubernetes全target Clippy全部退出0（`/tmp/rcoder-stop-404-{green,fmt,clippy,k8s-clippy,k8s-tests}.log`）。上一次workspace全量1916结果早于这两个分支，本次用受影响crate全量及消费方Clippy验证增补；完整严格Docker验收仍待执行。
- stop404及用户2小时默认值最终make dev-hot成功（`/tmp/rcoder-stop-404-dev-hot.log`），release增量构建3m05s；/proc/1/exe、/app/bin/rcoder、target-console产物三份SHA均0001bfe926d2a1882a4ecd12c66ff4e1e391679d4f88c3bb41849408c8dd6c73，HTTP/gRPC健康正常（`/tmp/rcoder-stop-404-running-hashes.txt`）。冻结本源码后执行完整严格验收，结果另记。
- 冻结主0001bfe9/runtime1cbdc0ee完整严格userApp run `68981e030cbd458e94aefab12868e611`命令退出0：33 passed / 0 failed / 0 skipped / 0 aborted，报告完整且前后源码指纹一致。Q10 fixture、双引擎热部署、7服务完整构建/生产发布、Docker身份、真实PG均通过；31个资源回执物理容器已不存在，既有app-cli-smoke身份/镜像/挂载不变，见该run的post-run-inventory.json。
- 最后Compose run `2cc1a22b79fc4ac38b03acf02042efed`受到并行环境/源码变更影响：16:59:53主服务重启，运行SHA由0001bfe9变为f126b971；启动清理删除正在创建的Agent容器，随后健康探测不可达。另有app-cli server/API、app_manager deploy_wait/start及full_chain测试新增工作区改动，源码指纹不一致。因此主动收束，21 passed / 2实际失败 / 1取消失败 / 30 aborted，不能算最终通过，也不提交混合状态。environment-change-evidence.json保存精简日志及运行身份；post-interruption-inventory.json确认24个已知case对应命名空间无容器残留，23个资源回执物理ID已不存在，既有应用保持不变。等待开发环境协调后再冻结验收；其他工作区改动完整保留。

## D01–D06 实施台账
基线：feature-userapp，保留开始时所有用户已提交及未提交变更。本批测试证据不得复用先前代码冻结结果。
- [x] D01 共享操作身份、冷等待与原子受理；组件及最终轻量URL/Docker部署链通过，见下方冻结验收。
- [x] D02/D04 app-cli 持久化恢复和 Docker/K8s 收敛；双引擎真实重启及K8s API契约通过，真实集群未执行。
- [x] D03 准备失败保留旧服务、切换后显式重部署及取消保护；两引擎各69条断言通过。
- [x] D05 输入校验和平台 env 隔离；组件与完整部署链通过。
- [x] D06 严格 E2E 必测映射、真实 A/B 与重启验证；最终33/33及54/54通过。
- [ ] 完整编译/测试、构建及 Compose 冻结验收；提交和 npm 发布仅在全部验收后执行。

### D系列组件证据（最终镜像验收尚未执行）
- 旧冷判据回归：`cargo test -p app_manager --lib lifecycle::deploy_wait`，1通过/3失败，实际复现身份混用、旧成功误放行及失败被健康掩盖；日志 `/tmp/rcoder-deploy-stage-red.log`。
- 修复后 app_manager 一轮127通过，日志 `/tmp/rcoder-unified-app-manager-final.log`；后续新增secrets/等待边界由完整workspace复验，不把此轮结果当最终冻结验收。
- app-cli 独立124测试通过，严格Clippy通过，日志 `/tmp/appcli-unified-test.log`、`/tmp/appcli-unified-clippy.log`。
- E2E Python工具59测试通过、部署Rust测试编译通过；真实Docker待新镜像。
- workspace默认Clippy通过，日志 `/tmp/rcoder-unified-clippy-workspace.log`；K8s/PG/完整测试继续执行。
- npm最新查询为0.3.3，本地app-cli/Cargo.lock已为0.3.4，无app-cli-v0.3.4 tag；本批仍使用0.3.4，发布等待全部验收。
- 构建预检：会被自动复制的四个builder脚本跨仓一致；不覆盖runtime Dockerfile既有Node24/Node22等工具链差异，Docker验收不等同生产工具链验收。

### 冻结代码门禁结果
- `cargo test --workspace`：1927 passed / 0 failed / 35 ignored（含文档测试）；`/tmp/rcoder-unified-workspace-tests.log`。
- `cargo test -p docker_manager --features kubernetes --lib`：140 passed / 0 failed / 5 ignored；`/tmp/rcoder-unified-k8s-tests.log`。实际API契约通过，未访问真实集群。
- 默认及K8s Clippy、PG feature all-targets check、workspace fmt check均exit0；日志 `/tmp/rcoder-unified-clippy-frozen.log`、`/tmp/rcoder-unified-clippy-k8s-frozen.log`、`/tmp/rcoder-unified-pg-check.log`、`/tmp/rcoder-unified-fmt-frozen.log`。
- app-cli最终全量124 passed / 0 failed / 0 ignored；`/tmp/rcoder-unified-appcli-final-tests.log`。最终状态机严格Clippy通过：`/tmp/appcli-final-state-clippy.log`。
- Python E2E工具59 passed：`/tmp/rcoder-unified-tools-final-tests.log`。
- 以上为代码/组件证据；真实Docker/PG严格验收、最终提交和npm发布仍待完成，不以组件绿灯代替。

- 最终重启边界复核：排除“Preparing无active回退env”与“准备失败第二次重启必失败”误报（当前已有保护）；确认并修复无附着operation改写foreign receipt、Existing首次hot缺失baseline及协调器scope依赖部署receipt三个缺口。app-cli 130 passed / 0 failed，严格Clippy/fmt通过，证据 `/tmp/appcli-coordinator-owner-{tests,clippy}.log`。新增回归覆盖连续重启、owner未知/不可读/提交失败。hot 增补真实 /ready 与前后身份fence、Failed持久化条件；最终镜像正在重建，尚未作为Docker验收通过。

- 最终 Workspace 复跑出现一项真实红灯：`ws_terminal::proxy::tests::relay_forwards_ttyd_data_and_injects_keepalive_when_idle` 在暂停Tokio时钟后等待真实TCP，被虚拟timeout先行触发，日志 `/tmp/rcoder-unified-final-workspace-tests.log`（该轮不是通过）。仅测试改用内存字节流上的真实WebSocket编解码，保留原10秒上限，增加29秒前不发送/30秒发送的时序断言，产品relay未修改。修正后的完整门禁另记。

- keepalive确定性测试修正通过：focused 1/1（虚拟时间0.00s），agent_runner全lib 248/248（16.07s），all-targets Clippy与diffcheck干净，证据 `/tmp/rcoder-keepalive-{focused,agent-lib,agent-clippy}.log`。仅 #[cfg(test)] 改动，生产转发逻辑未变化。最终Workspace完整复跑另记。

- 最终源码 Workspace 完整复跑退出0：1928 passed / 0 failed / 35普通环境ignored，含全部文档测试，证据 `/tmp/rcoder-unified-workspace-accepted-source.log`。默认/K8s Clippy通过（`/tmp/rcoder-unified-ready-{clippy,k8s-clippy}.log`），最后仅测试改动另有agent_runner Clippy通过。最终runtime构建退出0，镜像 `sha256:12ab4beaadfb28dd6e8ad56296dad81653ae77b8fdbfb17b93d85d2978677ad2`；`dev-restart`正在工具链检查，Docker严格验收仍未执行。

- 最终构建与热编译均退出0：`make docker-build-app-runtime`、显式runtime覆盖的`make dev-restart`、`make dev-hot`。主镜像6b0ae80a…、builder5e9d8105…、runtime12ab4bea…；主容器healthy，运行中/安装路径/target-console三份rcoder SHA一致为c804b95e…。两个镜像内app-cli实际运行版本均0.3.4，版本探针容器已定向清理。证据 `/tmp/rcoder-unified-{dev-restart,dev-hot}.log`、`/tmp/rcoder-unified-running-identities.json`、`/tmp/rcoder-unified-appcli-image-versions.json`。既有app-cli-smoke容器ID/镜像ID/每项挂载属性均未变；Docker返回的挂载列表顺序变动已按Destination规范化甄别，不是资源变更。严格E2E结果另记。

- 严格userApp run `180f940fd76c4caf965947ed89bb290a`：33/33全部通过、无skip/aborted/报告遗漏、源码指纹不变，含builtin/supervisord热部署、真实Docker删除身份、PG17故障与清理；轻量URL部署无release_id实际3.9秒确认，身份与流量断言通过。随后Compose run `e8114d91586d4d13b3ac70d5ed4e6fd9`：50pass/4fail，源码指纹不变；四项WebChat均HTTP500 ERR_CONTAINER_ERROR、Docker启动引用不存在的物理ID，保留原始失败报告，修复后须重跑。
- npm发布前发现本地builtin serve正常SIGTERM后owner仍Active、同namespace或非Linux无法再次serve（本地build命令确实推荐serve），不能以Docker新namespace重启通过排除此回归。正在增加显式Quiescent停机证明及真实native CLI红灯；当前33/33是补修前有效证据，不冒充最终版本验收。

- Compose WebChat 四项失败已定位为镜像 ABI：最终主镜像复用 Debian12/glibc2.36 的旧 master-base，而 rust:trixie 编译的 agent_runner 需要 GLIBC_2.38/2.39，进程退出后 auto_remove 导致后续 Docker404。主 rcoder 经 dev-hot 在旧运行层重编所以健康，不能排除内嵌 Agent 故障。正在补双二进制实际执行门禁及基础镜像来源检查；不是缓存失效回归。

- 正常停机补修最终通过：app-cli 138单测＋2真实native CLI集成测试、严格Clippy、fmt及diffcheck；旧实现真实正常SIGTERM→serve红灯保存在`/tmp/appcli-clean-shutdown-red.log`，最终结果`/tmp/appcli-clean-shutdown-{full,clippy}.log`。被拒启动正常退出不清旧Active，prepare取消等待真实blocking写入，错误/panic/超时保留保护。镜像双binary门禁工具加入后Python共62项通过；旧镜像ABI红灯位于Compose失败run的webchat-runtime-abi-red.json。新增修复后的镜像及完整E2E尚待完成。

- 新增停机/镜像门禁后的最终Workspace fmt、默认及K8s Clippy、全量测试全部退出0：1928 passed / 0 failed / 35普通环境ignored（含文档测试）。证据`/tmp/rcoder-final-{clippy,k8s-clippy,workspace-tests}.log`；Python62项通过`/tmp/rcoder-final-tools.log`。master-base重建中，Docker验收仍待执行。

- 补修后的镜像构建全通过：master-base、app-runtime、显式runtime的dev-restart、dev-hot均exit0；最终主镜像9a82e505…、builder78c2fa39…、runtime5632ab74…。主镜像最终层两binary实际运行门禁通过，运行层glibc2.41；健康healthy，运行中/安装路径/target-console三份SHA均b8bbe79c…；两个镜像app-cli实际版本0.3.4，身份探针无残留，既有app-cli-smoke ID/镜像/各挂载属性不变。证据`/tmp/rcoder-final-{master-base,app-runtime,dev-restart,dev-hot}.log`与`/tmp/rcoder-final-identities.json`。自此冻结源码和镜像执行最终严格验收，结果另记。

## 最终冻结验收（本轮有效结果）
- userApp：run `85af39005b3d43a6be3d5a0731071253`，严格33/33通过；[完整报告](../../tests-e2e/reports/85af39005b3d43a6be3d5a0731071253/summary.json)。两引擎各69条断言通过，无release_id轻量URL部署12.3秒确认，精确操作/制品身份与实际流量均通过。真实PG17与Docker生命周期通过。
- Compose：run `35a7bb063dc64341823cce57c6485875`，严格54/54通过；[完整报告](../../tests-e2e/reports/35a7bb063dc64341823cce57c6485875/summary.json)。含真实Anthropic/OpenAI、SSE终态/重连/并发订阅，以及此前失败的四项WebChat。
- 两次run均无failed/skipped/aborted/报告遗漏/清理错误，源码指纹前后均为 `ab7d564d14af58d0fcc7f78d822ac9bcb28bf1bf6289b8e010159ef087c727f1`。独立WebChat预回归run `22adaeba91a94fc2b2c8525acabfa2d0`亦4/4通过；旧ABI失败run原报告完整保留。
- 运行身份：两run的`post-run-identity.json`确认主容器/镜像/三份SHA不变、既有app-cli-smoke身份/镜像/全部挂载属性保留。共87份普通容器回执对应资源已不存在；`post-run-owned-resources.json`额外确认6组hot/PG/构建/生命周期专用容器、临时镜像、PG网络/卷及生命周期卷无残留。留证制品位于报告目录，不属于未清理应用资源。
- 最后PG feature all-targets check退出0，`/tmp/rcoder-final-pg-check.log`。Workspace1928项、K8s API140项（普通环境门控分别35/5）、app-cli138单测+2真实进程集成、Python62项及所有Clippy/fmt证据见前文。
- 验收结束后只补齐台账及将Python __pycache__统一忽略，避免新Docker检查脚本生成的字节码混入提交；业务和测试逻辑未变，原始run指纹及报告不改写。
- K8s仅编译/API契约，无真实集群操作；生产平台与app-runtime镜像需配套升级以启用协议4，本轮不推镜像或平台v*tag。npm主包+五平台包发布待下节记录，不以本地镜像0.3.4冒充已发布。
