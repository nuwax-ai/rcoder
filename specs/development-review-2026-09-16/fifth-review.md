# 第五轮复核：修复后的遗漏与验收边界

日期：2026-09-16。基线：`e419e2ba`，分支`codex/userapp-remove-user-id-binding`。

## 结论

本轮确认5项需要继续处理的问题。不能仅凭本轮remote K8s正常场景通过就关闭app-cli竞争时序、文件视图安全和重跑结果归集问题。

只读审查最新15个提交及相关调用链；没有修改业务代码，没有运行Cargo、Docker/Compose、SSH或K8s部署，不干扰正在运行的Compose回归。本报告新增文档不代表业务修复。K8s通过是用户提供的进度，本轮没有独立复核其完整报告及镜像身份。

## V01 / P1：重跑整轮失败可被case成功覆盖

位置：`tools/remote_k8s/main.py:678–714`（retest_failed）。

当前调用tests()失败后仅保存outcome.error，随后从summary的cases重新赋值verdict，未要求整轮summary.verdict=pass。因此，业务case通过，但末尾快照/部署身份/outside保护失败时，最终重跑仍记pass且still_failed为空，命令成功退出。刚修复的outcome.error KeyError不等于修好了这个判定。

另通过扫描所有summary的mtime猜测本次test_id不可靠；tests若在创建本次报告前失败，可能错误关联历史报告。tests本轮还会重新读取deployment.json，重跑应验证它与父报告身份一致，而非只在函数开始检查父身份一次。

**隔离实测：**伪造本次tests写入`verdict=fail,error=deployment identity changed,cases=[example:pass]`后抛RuntimeError。retest_failed正常返回，汇总得到`still_failed=[]`、case verdict=pass，同时还带着该RuntimeError。实验只使用临时目录及Python替身，没有调用真实集群。

修复：tests显式返回或通过可追踪异常提供本次run ID/完整结果，禁止扫描mtime推断；成功条件必须同时满足整轮成功、保护通过、所选case成功、身份与父记录匹配。任何执行异常都不得被case pass覆盖。保留失败错误及partial_coverage。

回归：case成功但快照变化、部署换代、outside变化、报告创建失败，均应失败退出；旧报告不得被误引用。

## V02 / P1：取消提交仍返回Passed，服务可在Cancelled后继续运行

位置：`crates/app-cli/src/server.rs:1486–1491,1740–1792`；`runtime_kernel.rs`的commit_execution取消分支。

本次修复已让kernel在admission锁内检查/写终态，但server把Committed和Cancelled合并为Passed，并清理current执行身份。若取消发生在外层current_operation_cancelled检查之后、commit_execution检查之前，kernel写Cancelled，server继续Running等待，未执行stop_all。这仍违反“取消结果与实际业务收束一致”。

修复：独立的取消清理结果分支，先确认业务/静态服务停止，再最终收束Cancelled并释放身份；清理未知保留RecoveryRequired。不能先不可逆地持久化Cancelled，再期待终态单调规则允许改成RecoveryRequired。builtin/supervisord都要覆盖。

回归：屏障控制外层检查→取消受理→内核提交，断言取消后业务停止、无成功事件、无丢失身份；清理失败时保留恢复保护。源码高置信推导，本轮未启动业务进程复现。

## V03 / P1：Stop仍覆盖kernel active，旧执行误入身份丢失且保护可能漏挂

位置：`runtime_kernel.rs:629–711,800–808`；`server.rs:1494–1520`。

Stop允许在A执行期间受理，但711行仍无条件将active_operation_id改为B。A提交得到NotActive，新的server分支把A写RecoveryRequired，却仍返回Passed；kernel.finish只有active==A才设置recovery_protection，此时active是B，所以保护可能没有开启。B后续正常完成清active后，新请求可以进入，而A的恢复记录仍未裁决。

这不是“旧NotActive直接放行”完全修复：现在虽增加错误记录，执行身份模型与恢复门禁仍不一致。

修复：明确区分执行者与待执行Stop；Stop推进revision不应抢走A执行身份。旧A不得按成功继续运行；恢复保护应由未知结果决定，不以当前active碰巧匹配为前提。

回归：慢启动A→受理Stop B→A提交→B停止→请求C，核验A/B各自终态、真实停止、无幽灵Accepted；A清理未知时C必须被拒绝。需要真实server循环测试，不只直接调用commit helper。

## V04 / P1：终态持久化失败仍被吞掉并释放server执行身份

位置：`crates/app-cli/src/server.rs:280–305`（finish_runtime_operation_by_id）。

kernel.finish失败时只写日志，随后仍清current_runtime_operation，函数返回unit；调用方无法获知未落盘。Stop路径还会先设Idle，再调用此函数。结果可能是业务已停止、API记录仍Accepted、server执行身份已丢失；失败收束流程也无法可靠知道是否已进入RecoveryRequired。

修复：终态函数返回Result；持久化失败保留执行身份并设置独立、可靠的server恢复门禁，调用者明确处理，不能日志后按成功继续。在状态记录无法写入时也需保持内存写保护，并让查询/健康面如实暴露问题。

回归：仅在终态store_operation阶段注入I/O错误，验证未成功收束、身份未丢、后续新旧入口均拒绝；恢复后有明确继续/裁决路径。

## V05 / P1：只保护.agents根，其他ACP根仍可沿链接删除外部目录

位置：`crates/file-server/src/service/agent_store.rs:731–742,759–784`；`service/skills.rs:258`。

新保护只拒绝workspace/.agents为symlink。循环实际处理`.agents/.claude/.opencode/.codex/.grok/.pi`。例如workspace/.claude指向victim，victim/skills是实体目录，linked_ok=false后remove_any_if_exists(workspace/.claude/skills)会沿祖先链接删除victim/skills，再创建视图链接。合法清单也可触发，修复Q02仍不完整。

修复：在任何manifest写入、create/prune前验证所有会访问的受管ACP根及必要store祖先；区别合法视图条目链接与可改变访问范围的祖先链接。错误查询不可当不存在；考虑检查后替换窗口，不能单加一次exists就认为安全。

回归：临时目录中分别将.claude/.codex等根指向victim，断言拒绝且victim、manifest、其他视图均无变化。保留合法skills条目软链兼容。本轮只核对源码，未执行删除请求。

## 已改善与仍待跟进

- Q01：已按规范化后值验证，并拒绝会被trim或components改变的标识；不重复报告旧空白点段缺陷。
- F04：TempDir生命周期和文件型agents删除已有修复，不再按原缺陷列为未修。
- K01/K02/K03：初始LIST缺席、写后错误分类、UTF8截断已有实质修改。本轮没有据此宣称全部故障时序验收完成。
- Q03/Q05：构建digest及各退出路径快照校验已有改善；重跑整体可信度仍受V01影响。
- R01–R06：释放成功身份、终态单调、先清理后Failed等已有改善，但V02–V04不能关闭。
- F02/F03仍在：skills.rs:101仍user_root.join(cid)，create/push共享模式仍取header上下文。继续按第三/四轮要求贯穿resolved workspace/context。
- Q07共享视图锁、R07/R08稳定根/Source计划、K04/K05统一预算/Event关停仍需对照原任务补齐；本轮未全面重复验证这些项，不自动勾选。

## E2E契约记录需要修正

`8a40656b`将HTTP无URL start的测试改为忙锁Conflict；实际HTTP调用链确实是handler→start_app_enhanced→deploy_controlled(try锁)，而start_app_controlled是另一条等待入口。源码显示这是已有路径区别，不能把该测试修改直接定性为本轮生产行为回归。

但前面移交把“无URL start等待”当作对外语义，存在描述不准确。必须在verification中明确：HTTP enhanced入口与controlled入口分别测了什么，等待型入口是否仍被实际调用；不应仅凭注释“对齐契约”认定已经满足原对外要求。不得为了测试通过擅自改变生产锁语义，也不要机械恢复错误断言；若要统一对外语义，另行依据用户确认的需求处理。

## 本轮验证

- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/remote_k8s/tests -v`：36/36通过，退出码0。日志`/tmp/rcoder-quality-round5-python.log`。
- V01隔离反例正常返回但错误标pass，说明现有36例未保护该场景。
- V02–V05为源码推导，未执行Rust构建或真实业务故障注入。
- 不将用户提供的K8s通过进度表述为本轮独立验收，不用它替代上述竞争/异常反例。
