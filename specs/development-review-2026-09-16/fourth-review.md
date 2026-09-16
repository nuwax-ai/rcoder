# 第四轮代码质量复核：修复遗漏与当前优先级

日期：2026-09-16。源码基线：`092c05dc97550285c1beec61ae7ad1c10d1d6234`。
分支：`codex/userapp-remove-user-id-binding`。

## 结论与范围

本轮没有发现理由将项目标为“整体无问题”。部分修复已到位，但第三轮的运行内核与K8s状态机缺陷仍在；新增技能名校验和远端工具修复也存在遗漏。优先处理下面Q01、Q02，以及第三轮R01–R06、K01–K03。

本轮检查了近期提交、app-cli提交/取消/清理、file-server共享视图与ZIP、K8s观察与上层恢复保护、远端构建/测试/诊断，并抽查agent_runner终端目录、proxy及ZIP安全入口。属于跨模块重点源码审查，不是逐行穷尽全部仓库，不代表未列模块均已验证安全。没有把测试中的unwrap计为生产缺陷，也没有把单纯风格偏好列为问题。

审查时Claude在执行remote K8s测试，工作树仅`tests-e2e/tools/k8s_userapp.py`有在途修改。本轮未改业务代码、未运行Cargo/镜像构建/部署/SSH或集群测试，未介入其测试进程。该在途文件不纳入稳定实现完成判断。

## 一、新发现及修复未闭环

### Q01 / P1：F01校验后再trim，危险路径可重新产生

位置：`crates/file-server/src/service/agent_store.rs:40–53,681–699,765–775,784–843`。

新校验检查原字符串的Path components；`".. "`、`" . "`在Unix上都是合法Normal组件。但后续normalize_names会trim，变成`..`或`.`，未经再次校验就写manifest、组成反向引用并参与视图清理/重指。下一轮read_manifest又会拒绝这些已落盘非法条目，导致项目持续报错；本轮校准还可能先清掉合法技能视图。不能将原始字符串校验视为最终使用路径的安全保证。

此外，校验器放行`a/`、`a/.`，但使用方保留原字符串作为manifest键；目录枚举返回`a`，不匹配引用键，导致反复删除重建，且带末尾点段的链接处理不应假定与普通文件名完全等价。

修复：只生成一份规范化后的合法单段标识，在任何文件变更前完成规范化、校验及去重，后续全部使用该值。或者明确拒绝会被规范化改变的输入；兼容选择以原需求为准。读旧manifest同样处理，不允许“校验一种值、使用另一种值”。

验收：`".. "`、`" . "`、Unicode空白包围点段、`a/`、`a/.`；断言拒绝或合法规范化符合契约，manifest/已有视图/业务数据均不被错误改变。此项为源码推导，未在真实目录执行破坏性请求。

### Q02 / P1：F01只保护名字，未保护可被链接替换的祖先目录

位置：`agent_store.rs:701–723,784–800`。

共享视图代码只检测`.agents/skills`是否为链接，没有检查workspace下`.agents`祖先本身。若`.agents`是指向其他目录的链接，`create_dir_all`/`read_dir`沿链接访问目标，随后“无引用条目清理”对目标目录中的条目调用递归删除。合法skillNames也不能阻止这种越界。

修复：在写/prune之前建立受管目录身份，核验祖先及目标类型，拒绝非授权链接祖先；删除/替换时保持同一边界，考虑检查与使用间替换窗口。普通条目链接与受管根链接区别处理，不能全面禁用正常ACP视图链接。

验收：只用临时目录，将workspace/.agents链接到临时victim（含skills/keep/data），触发空/普通清单同步，断言明确拒绝且victim原样保留。源码高置信推导，未执行真实删除。

### Q03 / P1：T06声称禁用缓存，实际仍读写相同失败key；构建未使用已解析digest

位置：`tools/remote_k8s/main.py:193–213,244`；`build_cache.py:24–43`。

两处问题：

1. 解析失败只写`unresolved:<tag>:<异常类型>`，仍无条件load缓存。上次同样失败但实际build成功写入缓存后，下次同类失败key相同，可复用旧产物。打印“reusing cached builds is disabled”与执行相反。
2. 即使解析成功，构建参数RUST_IMAGE仍传配置tag，未传receipt中的固定digest。解析和构建之间tag变化可使receipt/cache标识与实际工具链不一致。

修复：解析失败明确失败，或禁用该轮缓存读写并如实记录不可复现性；优先解析失败即停止。成功后传同一digest给实际build，并写receipt。不能只给失败key添加字样。

隔离实验已确认两次相同unresolved输入得到相同key；是否命中由当前无条件load及store调用链判定，未连接registry或构建镜像。

### Q04 / P1：T05只修了catalog，失败重跑仍绕过收束保护且覆盖历史

位置：`tools/remote_k8s/main.py:578–654`（retest_failed）。

仍用`<parent_id>-retest/<case>`固定目录并exist_ok，多次重跑覆盖报告；执行后未复用普通tests的identity/pod/outside检查；catch BaseException后继续下一case，Ctrl-C也可能被吞；未将父报告test_source_sha256与重载快照记录作绑定核验。

修复：统一普通运行与重跑执行/收束路径，新run ID、父报告关联、完整身份校验，取消立即结束。保留partial_coverage，不改变父报告。此项为第三轮T05续项，不是新功能。

### Q05 / P2：T03执行后校验仅在成功路径，失败/取消绕过

位置：`main.py:489–512`（execute_suites）。

新增verify放在函数末尾，suite抛异常就不会执行；外层finally只写报告。结果虽不会直接变pass，但无法确认失败对应哪个输入，也无法可靠区分输入篡改与业务失败。

修复：在所有收束路径校验冻结输入，保留原始错误与校验错误，不互相覆盖；不让校验异常掩盖中断。Python字节码等输出须定向到快照外或明确禁用，不能靠忽略输入变化解决。

隔离实验：mock suite抛受控异常，verify只调用1次（执行前），证实失败路径缺末尾校验。

### Q06 / P2：T07诊断结果与总预算仍未完整落实

位置：`tools/remote_k8s/diagnostics.py:55–75,145–158,171–209`；`main.py:660`起logs。

- Ceph查询无健康值时返回普通字符串，装饰器仍记pass；应unknown，空观察不能证明健康。隔离SSH替身返回空字符串已复现pass。
- as_completed超时后又遍历所有done future追加，已收集的结果重复进入报告；应仅收集pending中尚未记录者。
- pool.shutdown(wait=False)不能终止正在执行的SSH；Python退出仍可能等待线程。部分探针未传单项timeout，不能称整个命令已受120秒约束。
- logs的namespace/pods/events前置读取仍在逐项保护外，events失败会阻止后续容器日志收集。

修复：一项一结果，unknown真实表达缺观察；所有底层调用受剩余预算约束并有可收束机制；日志每项独立保存错误，原业务错误保留。

### Q07 / P2：共享视图锁过期接管没有身份保护

位置：`agent_store.rs:605–650`（ViewGuard）。

锁文件5分钟无续约即删除；旧持有者可能仍在大目录复制/慢存储I/O。B删旧锁后创建新锁，A的Drop又无条件remove_file删除B的锁，C可以同时进入；manifest与视图会并发覆盖。metadata读取错误也直接当stale，错误不证明锁无主。

修复：明确可证明的持有者生命周期和锁所有权，使用适合当前共享存储的锁机制；不能仅靠mtime判断安全接管。释放必须对应自身持有权，不能删除后来者。测试A超时仍活/B接管/A释放/C竞争，断言始终单写者；故障注入缩短测试时间，不真实等待5分钟。

## 二、上一轮问题当前状态

旧问题细节见[第三轮报告](third-review.md)，这里保留原ID，避免重复建任务。

| 原ID | 当前核对结果 |
|---|---|
| R01–R06 | 未闭环。app-cli相关文件从第三轮基线至今无变更；抽查commit_execution仍drop admission guard后finish，server仍把Cancelled/NotActive当Passed，fail_activation仍先finish后cleanup。执行ID释放、排队Stop与取消、恢复保护问题继续优先处理。 |
| R07–R08 | 稳定根真实接入、请求级Source执行计划仍未完成；不能以配置消费者/拒绝未实现组合视为完整功能。 |
| F01 | 已增加原始路径段校验与旧manifest校验；但Q01/Q02使安全闭环仍未完成。 |
| F02–F03 | 仍存在。service/skills.rs:101仍user_root.join(cid)；create/push仍按header任务上下文决定共享分支，没有贯穿resolved context。 |
| F04 | 仍存在。computer_ws/agent_store_ws.rs:132创建TempDir，179析构，240才消费agents源；update_agents_dir仍将源不存在当无输入跳过。 |
| F05 | 本轮未逐条复验全部退役字段文档，继续保留原待办，不宣称关闭。 |
| K01 | 仍存在。k8s_observation.rs:402起忽略Init/InitDone，只有Delete产生PodAbsent，初始LIST空会超时。 |
| K02 | 仍存在。k8s_builder_control.rs:378/407写后verify仍映射rejected_before_write；rcoder/userapp_builder/control.rs:272起据RequestRejected释放保护。 |
| K03 | 仍存在。k8s_event_publisher.rs:66–80先找UTF8边界再减3；257个emoji截断点1021不是字符边界。 |
| K04–K05 | 相关文件未变更，最终GET统一deadline、Event可等待关停/计数闭环仍待处理。 |
| T01–T02 | Make变量传递、业务入口report引用已有修复；本轮工具测试通过。不据此认定所有真实业务套件已验收。 |
| T03 | 成功路径及文件内容指纹有改善，失败收束遗漏见Q05。 |
| T04 | 非零退出读取日志并保留cases已有实现；仍依赖stdout正则，不是原要求的结构化报告。异常种类、空/缺日志与中断应继续补验，不能把这部分实现算完整结果协议。 |
| T05–T07 | 部分修复，Q03/Q04/Q06继续跟进。 |
| prod锁快失败 | d38ed66c实现已复核；94550967补齐restart OpenAPI。保留上轮本地验证结论，当前实际K8s/Java接入结果等Claude本轮报告。 |

## 三、实际验证与边界

- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/remote_k8s/tests -v`：33/33通过，退出码0。日志：`/tmp/rcoder-quality-round4-python.log`。
- 隔离Python实验：失败suite仅前置verify、unresolved缓存key相同、空Ceph观察pass。均不调用真实SSH/集群，不代表真实E2E。
- Rust问题来自当前源码调用链核对；未运行破坏性复现或Cargo，不将源码推导写成测试已复现。
- 没有执行/替换正在运行的K8s测试；也未核验本轮集群最终报告。性能、完整安全审计、所有crate逐行覆盖不在本轮已完成证据内。

## 四、处理顺序

1. 先Q01/Q02数据保护、R01–R06状态机、K02写后恢复保护、K03非致命诊断、K01初始缺席。
2. 完成Q03–Q06工具可信度，再依据实际suite/case及固定输入采信远端结果；现有测试结果保留原基线，不覆盖。
3. F02–F04共享视图完整调用链、Q07共享锁，以及R07/R08原需求剩余项。
4. 各问题补修复前可失败的反例；重点为非法输入零变更、双操作真实时序、写后失败保护、实际Make/runner报告链。避免只测helper或字符串返回值。
5. 实现完成后再安排默认/全features nextest、app-cli独立测试及Compose/K8s受影响回归。不要在当前共享测试运行中插入另一次部署。
