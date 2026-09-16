# Claude Code：第五轮修复与验证

请在RCoder仓库继续修复第五轮审查问题：

`specs/development-review-2026-09-16/fifth-review.md`

完整阅读AGENTS.md及该报告，结合第三/四轮报告和原Spec。第五轮基线e419e2ba；先检查当前分支、git status、相关diff及后续提交，不覆盖已有改动或重复修复。

## 必做

1. V01：retest结果必须以本轮tests完整结果为准。整轮失败不能被case pass覆盖；显式传递run ID，禁止mtime猜测；保护父报告身份及失败信息。
2. V02：Cancelled不能当Passed继续运行；取消提交与实际停服/清理保持一致，未知结果保留RecoveryRequired。
3. V03：执行身份与排队Stop分离；Stop不得抢走旧执行者身份，恢复保护不依赖active恰好匹配；覆盖A启动/B停止/C新请求真实时序。
4. V04：终态持久化失败向上传播，保留执行身份及可靠恢复门禁，不允许仅日志后清身份/继续成功流程。
5. V05：保护所有ACP根及相关祖先，阻止沿.claude/.codex等链接删除外部目录；合法视图链接保持兼容。
6. 核对报告中的未完成旧项，更新逐项状态。已修项目不重复报未实现，部分实现不勾成整项完成。
7. 澄清HTTP enhanced start与start_app_controlled两条入口的真实锁语义及测试覆盖；不要根据测试成败自行改需求或扩大生产行为变更。

## 验证纪律

- 先补报告指定反例，再修复并验证。数据破坏性输入只用临时目录，断言拒绝后零变更。
- 生命周期测试到真实server执行/清理与持久化边界；不能只测试helper或状态值。
- V01必须覆盖case通过但身份/快照/outside校验失败，确保最终非零退出；报告不能误关联历史run。
- 当前若Compose回归仍在运行，先保全其源码身份和结果，不替换其部署、不并发Cargo共用target。必要时用隔离工作区进行独立检查。
- 优先nextest，先聚焦再按影响扩大；app-cli独立fmt/clippy/nextest；受影响默认和全features分别验证。
- 修复后执行受影响Compose及个人K8s回归，使用.env.local和make remote-k8s-*，不操作19/生产集群、不删除agent PVC、不写入凭据。
- E2E通过必须关联实际源码快照、镜像digest、suite/case及完整报告。未覆盖竞争场景、缺配置、环境失败均如实记录，不能用smoke替代。
- 普通实现与已授权的可逆验证持续完成，不逐项请求批准；不push，不git add -A，提交仅精确暂存本任务改动。

追加verification，按V01–V05列问题核验、修复内容、修复前后反例、命令/退出码、证据和未完成项。区分组件测试、Compose、K8s与外部接入验收；若某条审查不成立，用完整调用链和测试证据说明。
