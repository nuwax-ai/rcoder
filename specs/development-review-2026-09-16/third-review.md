# 第三轮开发成果审查与修复清单

日期：2026-09-16。基线：`511c5c1b6d3106273b097fce4722c1ccf6578dc1`。

## 1. 结论与证据边界

当前不能宣布前面任务全部完成。运行内核、共享技能视图、K8s 观察和远端工具都有实际新增实现，但存在数据保护、状态机及验收可信度缺陷。优先修复 F01、R01–R06、K02；在采信远端业务验收前修复 T01–T03。

本轮分四条线审查实际调用链：app-cli；file-server/UserApp及TS对照；K8s运行时；远端构建/测试工具。未修改业务代码、未运行 Cargo、未操作集群或干扰正在执行的 E2E。以下源码行号属于上述基线，修改后应按符号定位。

证据分级：R/F/K 为高置信源码推导，尚未运行触发场景；T01/T02/T03/T07 有隔离 Python/Make 实验，其余为源码推导。历史 verification 中的通过数量不视为本轮实测。

本轮实际执行：

- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/remote_k8s/tests -v`：24/24通过，退出码0。
- 临时 Makefile include 当前 `make/remote-k8s.mk`，传 `SUITE=chat CASE=test_x RUN=<32位值>`，检查导出环境：仍为 `REMOTE_K8S_SUITE=smoke`，CASE/RUN为空。退出码0。
- 用最小伪配置直接调用 `run_chat_suite`、`run_userapp_suite`：均捕获 `NameError: name 'report' is not defined`，未发起网络或测试。
- 临时测试源与manifest：修改源文件后，启动器 `source_fingerprint` 前后相同。
- 诊断SSH替身返回 `HEALTH_ERR`：ceph-detail结果仍为pass。

隔离实验中的替身只验证工具控制逻辑，不是业务/真实AI E2E证据。24个现有工具测试通过不能覆盖以上入口缺陷。

## 2. app-cli 运行内核

### R01 / P1：成功后未释放 ServerState 执行身份

位置：`crates/app-cli/src/server.rs:243,1427`；`runtime_kernel.rs:800`。

`commit_running_barrier` 直接通过kernel提交成功，清了kernel active，却没有经过清理 `current_runtime_operation` 的server封装。Start A成功后Restart B被受理，设置执行身份B遭旧A拒绝，完成时仍查询A；B可能永久Accepted并阻塞后续操作。

修复：统一终态提交和按ID释放执行身份；明确kernel/server两份身份的一致性。回归必须驱动实际server循环，顺序执行Start→Restart→Stop→Start，逐一断言执行ID与终态。

### R02 / P1：Stop受理抢占active，旧启动越过提交屏障

位置：`runtime_kernel.rs:711,830–854`；`server.rs:1427`。

A启动中受理Stop B，active改成B；A提交先得到NotActive，未进入revision检查，server却将非Superseded结果视为Passed。A可进入Running且原操作未正确结束。

修复：区分受理/排队身份与执行身份，NotActive不可等同提交成功。测试慢启动A→Stop B→A返回，断言A不能越过屏障、B完成且无遗留Accepted。

### R03 / P1：取消与成功提交不原子，Cancelled仍可能运行业务

位置：`runtime_kernel.rs:830–880`；`server.rs:1427`。

提交检查释放锁后再finish；取消能在两者间写入。server将包括Cancelled在内的非Superseded结果当Passed，可能API已取消而业务继续运行。

修复：明确取消受理与成功提交的线性化点，区分Succeeded/Cancelled/NotActive/持久化失败，并驱动相应清理。用可控屏障覆盖提交前、提交交界、提交后三种时序。

### R04 / P1：排队取消变成Stop，Cancelled被覆盖为Succeeded

位置：`server.rs:1284–1287,1505–1525`；`runtime_kernel.rs:775–785`。

取消收束后返回同ID的StopBusiness，随后执行Stop并再finish成功；finish允许覆盖既有终态。

修复：引入无需业务动作的已收束分支；终态单调不可覆盖。测试取消排队启动不停止无关运行实例，且Cancelled持久记录保持不变。

### R05 / P1：初始化/期望状态读取失败后的恢复保护可被绕过

位置：`server.rs:520–525,871–879,889,914–940`。

kernel初始化失败时仅局部hold且kernel=None，旧API只检查Some(kernel)；后续ownership claimed可能重开入口。desired读取失败虽改Idle，却未清除预先生成的first_request，仍可能自动启动。

修复：建立server级统一恢复保护；可信状态读取前不得生成可执行恢复动作；新旧API/环境启动/后台恢复共用门禁。覆盖损坏、不可读、kernel缺失与旧入口请求。

### R06 / P1：失败先终态后清理，清理未知可能失去保护

位置：`server.rs:1381–1395`；`runtime_kernel.rs:800–805`。

fail_activation先finish Failed并清ID，再drain/cleanup。后续清理失败失去原操作身份；kernel又只在active匹配时设置恢复保护，旧A清理不明而B已active时也可能漏保护。

修复：保留操作身份直到清理确认；结果未知对相应运行态建立保护，不依赖“当前active恰好仍是该ID”。测试清理失败、清理中Stop受理、持久化失败后的新旧写入口。

### R07 / P1（未完成）：稳定状态根没有贯穿真实启动链

位置：`runtime_kernel.rs:93–102`；`server_journal.rs:70–78`。

有APP_CLI_STATE_ROOT消费者，但未找到平台/容器实际注入；parent回退仍使源码目录与`.run`使用不同锁域。配置能力存在不等于所有权统一完成。

修复：在实际平台启动、容器配置及直接CLI约定中落实同一稳定根；覆盖源码态和制品态并发竞争同一锁，以及目录换代后恢复。

### R08 / P1（未完成）：请求Source profile没有落实到执行计划

位置：`runtime_kernel.rs:727–732`；`server.rs:1289`；`supervisord_host.rs:179`；`supervisor.rs:541`。

请求source参数未随执行传递；dispatch仍用Existing，实际dev行为依赖serve进程环境变量。正常serve收到Start+Source不能据此保证执行devrun/devbuild。

修复：生成并传递每次操作的ResolvedRunPlan，包含profile、cwd、命令、构建与探测；builtin/supervisord行为一致。以devrun和run明显不同的fixture验证真实命令。ArtifactId解析及平台阶段三仍需按原规范继续，不能把拒绝未支持请求算实现完成。

## 3. file-server / UserApp

### F01 / P1：技能清单路径穿越可递归删除业务目录

位置：`crates/file-server/src/handlers/computer/workspace/create.rs:172,195,252`；`service/computer_ws/agent_store_ws.rs:245`；`service/agent_store.rs:703,747,758–775,310`。

外层仅将skillNames解成字符串列表；normalize_names只trim/去重。清单项直接join，找不到实体时递归删除目标。`skillNames=["../../victim"]`可将受管视图路径解析到workspace/victim；对应store实体不存在时可删除业务目录。外层未找到路径段校验。

修复：在任何写/prune前验证skill/subagent名和agent ID为合法单路径段；持久manifest读取同样校验；删除/替换还须核验受管范围与对象类型，拒绝链接逃逸。临时目录回归断言恶意请求失败且无任何业务数据改变，覆盖绝对路径、点段、分隔符和损坏旧manifest。

### F02 / P1：push丢弃已解析workspace，写入错误目录

位置：`handlers/computer/workspace/push_skills.rs:90`；`ops/workspace.rs:137`；`service/skills.rs:101`。

handler解析正确ws，底层却重算user_root.join(cid)。normalProject及显式绑定目录会成功更新别处，实际项目未更新。

修复：PushToStoreParams显式传已解析session_workspace，不反推目录。通过真实请求断言目标项目视图更新且旁路目录未创建。

### F03 / P1：共享视图分支未使用合并后的定位上下文

位置：`handlers/computer/workspace/create.rs:262`；`push_skills.rs:121`；`handlers/computer/mod.rs:219`。

路径解析支持合并输入，但共享分支只看header类型和原始body appId。body-only normalProject或header-only appId可正确定位却走单agent视图/错误store根。

修复：入口解析一次resolved context，统一用于workspace、store根、项目manifest。覆盖header-only/body-only/冲突优先级及两个agent先后与并发访问。

### F04 / P1：ZIP临时目录提前释放，subagents上传假成功

位置：`service/computer_ws/agent_store_ws.rs:110,154–157,218`；`service/agent_store.rs:203–212`。

TempDir guard在消费agents前析构，仅留下不存在的PathBuf；update_agents_dir把不存在源当无输入跳过，接口仍成功。另同名.md更新路径对旧文件使用remove_dir_all也需修复验证。

修复：guard存活至全部消费完成；明确存在的上传源丢失应失败。真实create service/handler上传agents/*.md ZIP，核验store/manifest/ACP视图，并再次上传更新同名文件。

### F05 / P2：退役user_id仍误导OpenAPI消费者

位置：`crates/rcoder/src/userapp_forward/forward.rs:241,273`。

utoipa请求描述仍写“仅需user_id与project_dir”。更新生成文档及相邻旧说明，断言UserApp不再要求user_id/x-user-id；保留普通Computer的用户隔离契约。

## 4. K8s运行时

以下路径均在`crates/docker_manager/src/runtime/`，另注明者除外。

### K01 / P1：Stop初始LIST为空时等待到超时

位置：`k8s_observation.rs:402–408`；`k8s_builder_control.rs:300–344`。

PodAbsent只由Delete产生；初始LIST已为空或410后relist为空不会生成完成候选。已成功停止误判RecoveryRequired。

修复：跟踪完整Init/InitApply/InitDone快照，在快照结束后判断缺席，再权威复核；覆盖初始空、快速删除、410重列举、替代Pod。STS消失明确分类。

### K02 / P1：写后冲突错标写前拒绝，释放保护

位置：`k8s_builder_control.rs:378,407`；`crates/rcoder/src/userapp_builder/control.rs:272–288`。

PATCH/DELETE成功后最终verify_workload_stable错误映射rejected_before_write，上层finish_external_mutation释放mutating并写Failed。UID替换/副本被改等写后冲突应保持未知结果保护。

修复：按写入阶段分类错误；写后复核冲突保持RecoveryRequired。通过上层control worker测试数据库状态、租约/保护及后续写拒绝，不能仅测错误helper。

### K03 / P1：Event UTF-8截断可同步panic

位置：`k8s_event_publisher.rs:70–79`；`k8s_builder_control.rs:180–190`。

先找边界再减3字节预留省略号，会再次落入字符内部；`"😀".repeat(257)`使truncate(1021) panic。事件在业务返回前构造，破坏诊断非致命要求。

修复：先预留字节再找合法边界，覆盖2/3/4字节和边界。处理同文件187生产expect及锁中毒，使诊断错误不影响业务。

### K04 / P2：总deadline未覆盖最终GET

位置：`k8s_builder_control.rs:300,350,367–398`。

watch有90秒deadline，后续权威GET无timeout_at；接近截止时出现候选可继续无界等待客户端。

修复：观察/重试/最终复核共用绝对deadline；GET hang/429回归断言总耗时及保护状态。

### K05 / P2：Event关停预算、在途计数和生产观测未闭环

位置：`k8s_event_publisher.rs:142–145,207–225`；`kubernetes_runtime.rs:152`。

在途publish最多3秒后才进入5秒排空；超时取消的在途项未计入remaining。spawn句柄丢弃，无生产可等待shutdown；计数生产侧被弃置。

修复：可等待且总预算明确的shutdown，记录在途未知结果，公开可用计数。覆盖发送已开始时取消和计数守恒；不把允许10秒的测试当5秒门禁。

## 5. 远端开发测试工具

### T01 / P1：Make公开参数未传递，业务验收可能只跑smoke

位置：`make/remote-k8s.mk:5–12`；`tools/remote_k8s/main.py`参数解析。

文档使用SUITE/CASE/RUN，Make仅导出REMOTE_K8S_*且无映射。隔离实验已确认SUITE=chat仍导出smoke。

修复：安全映射公开参数，定义与显式REMOTE_*冲突时优先级；兼容系统Make，不使用shell字符串拼接。测试必须经过真实Make→Python参数入口，覆盖all/chat/userapp/gateway、CASE/RUN和特殊字符，不能只测Python parser。

### T02 / P1：Chat/UserApp入口引用未定义report

位置：`tools/remote_k8s/main.py:411,434`。

两入口均在启动业务测试前NameError，已隔离复现。改为正确context，并通过入口级测试断言冻结路径、manifest、输出路径完整传递。

### T03 / P1：测试后未核验冻结源，manifest指纹不检查文件

位置：`tools/remote_k8s/main.py:463,520–538`；`tests-e2e/tools/run.py:129–136`。

仅执行前调用快照verify；launcher有manifest时只hash清单JSON，源实际变化仍保持指纹，已隔离复现。

修复：在成功/失败/中断收束时重新验证实际文件内容、集合、模式及链接；校验失败不得pass。输出/Cargo缓存和Python字节码写到快照之外（或禁字节码），不能以放宽输入校验解决污染。测试中修改活动源码应不影响冻结运行；修改冻结源则必须失败。

### T04 / P2：失败case无法写报告，失败重跑缺输入

位置：`main.py:419–423,520–538,560–563`。

run在非零退出时抛异常，case解析与context写入仅成功路径执行；真实失败报告缺failed cases，retest拒绝运行。

修复：无论退出码均读取严格启动器结构化结果，区分场景失败/基础设施失败/取消；无报告不可伪造case。用受控失败验证父报告保留失败case并可重跑。

### T05 / P1：重跑绕过收束校验且覆盖历史

位置：`main.py:559–607`。

retest只前置核验身份，直接调用execute_suites后pass，未复用普通路径末尾身份/外部资源等检查；BaseException捕获后继续下一case。目录固定为parent-retest/case，多次覆盖；catalog读活动ROOT而非父快照。

修复：复用统一有保护的执行收束路径；唯一新run ID，绑定父报告/父test_source_sha与冻结catalog；中断立即终结，历史不可覆盖。覆盖运行中部署变化、重复重跑、活动catalog变化及Ctrl-C。

### T06 / P2：缓存未锁定Rust构建镜像digest

位置：`tools/remote_k8s/build_cache.py:25–35`；`main.py`构建参数RUST_IMAGE。

key使用可变tag字符串，而非实际构建工具链镜像digest；tag更新仍复用旧产物。修复时解析并固定实际构建digest，纳入key及receipt，核对registry/image prefix切换时的复用契约。覆盖同tag不同digest必须失效。

### T07 / P2：分层诊断可假通过，总预算与部分收集不完整

位置：`tools/remote_k8s/diagnostics.py:80–84,139–161`；`main.py`日志收集与status路径。

Ceph HEALTH_ERR仍pass（已复现），deployment只检查存在不检查ready。线程池超时退出可能等待线程且来不及落报告；部分底层调用预算长于总预算。权限不足不统一unknown。logs部分前置查询仍可阻断后续独立收集。

修复：按真实条件判pass/fail/unknown，每项有剩余预算，始终落部分报告；日志逐项失败不阻止其他项。status按时间选报告并核验当前部署身份，不能仅凭相同源码SHA称当前验证通过。覆盖HEALTH_ERR、零ready、403、慢请求和单项日志失败。

## 6. 前面任务实际完成度

| 任务 | 当前已有实现 | 尚未闭环 |
|---|---|---|
| 运行态单一所有者 | 旧B01 Idle Stop ID、B02重复poll修复存在；持久操作/profile/取消框架已落地 | R01–R08；ArtifactId；平台阶段三；真实双引擎时序验收 |
| UserApp移除user_id/x-user-id | 模型及转发主体已退役，upstream显式移除header | F05；无header完整链路/旧数据升级/普通Computer隔离需实测 |
| TS文件服务同步 | workspaceType回退已删，转发补header；normalProject manifest并集已实现 | F01–F04；UserApp共享视图源码明确暂缓，应回对原范围，不可称全部同步 |
| kube-runtime | A/B/C均已有实现；旧B07外层退避及客户端重试隔离有改进；三个RBAC来源已有Event权限 | K01–K05；真实UserApp/Chat与故障时序；D为明确可选，不擅自列成本轮必做 |
| Docker list缓存 | 代次提交/失效共锁及延迟404保护存在，抽查未发现同类新缺陷 | 本轮未重跑默认Docker回归，不能据此整体验收 |
| 远端开发体验优化 | 快照/缓存/status/check/筛选/retest代码存在，24个工具测试通过 | T01–T07；目标级输入闭包/耗时基线等tasks仍待办；真实冻结运行/缓存/失败重跑验收 |
| 开发规范与Make | nextest、Compose/K8s分层门禁有文档和入口 | 当前脏改动尚未作为本轮完成交付核验；本报告不替其背书 |

进度文档需同步：ownership、remove-user-id、kube-runtime的tasks仍有“仅规划/全未实施”等旧表述；remote优化tasks虽已有勾选，部分勾选与T01/T03/T06/T07矛盾。应按需求分别标实现、组件验证、部署验证，保留历史verification基线，追加本轮记录。不要批量勾完，也不要删除尚未实现的原需求。

## 7. 修复顺序与验收

1. F01数据保护；R01–R06状态机/保护；K02写后归类和K03非致命诊断。每项先补能暴露错误的反例。
2. T01/T02恢复真实业务入口，T03/T05保证结果可信，再修其余工具问题。既有远端报告须核对实际suite，不能仅信命令文字。
3. F02–F04、K01/K04/K05及R07/R08真实链路；落实原Spec剩余必做，D等明确可选独立列出。
4. 聚焦nextest后验证受影响默认features及全features；app-cli独立检查。共享Cargo target串行。
5. 修复稳定后执行适当Compose核心业务、远端smoke/gateway/userapp/chat，记录实际suite/case、源码摘要、镜像digest、namespace及完整报告。缺LLM或环境故障明确受阻，不替换成smoke通过。
6. 保存每项“修复前反例、修复后结果、剩余范围”。本轮文档不是代码修改或部署验收。
