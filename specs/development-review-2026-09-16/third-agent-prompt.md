# Claude Code 开发与验证提示词

请继续完成RCoder前面任务，依据以下第三轮审查修复实际缺陷并完成原规范剩余必做项：

`specs/development-review-2026-09-16/third-review.md`

审查基线为511c5c1b。先读取AGENTS.md、git status及相关diff；若源码已变化，逐项核对问题是否仍存在，不重复覆盖已修内容，保留无关改动。当前测试若仍运行，先保全其报告与版本身份；不要并发替换同环境部署或共用Cargo target。不要仅按历史报告的通过数量宣布完成。

## 工作范围和顺序

1. 首先处理F01路径穿越/数据删除；R01–R06执行身份、Stop/取消竞争、终态覆盖、恢复保护和清理顺序；K02写后错误归类及K03 UTF-8 panic。要求统一状态机和保护边界，不只为单个helper加特殊判断。
2. 修复T01–T07远端工具：真实Make参数传递、业务入口NameError、冻结源执行前后校验、失败case持久报告、受保护且不可覆盖历史的retest、构建镜像digest缓存、可信诊断。必须经过公开Make入口验证，禁止把SUITE=all/chat静默降成smoke。
3. 修复F02–F05、K01/K04/K05；完成R07稳定根在真实启动链的接入、R08每操作Source执行计划。两个运行引擎都应落实devrun/devbuild语义，不能依赖全局环境变量伪装请求profile实现。
4. 对照userapp-runtime-ownership、userapp-remove-user-id-binding、kube-runtime-adoption、TS同步相关规范和remote-k8s-dev-optimization逐项盘点。ArtifactId、平台迁移阶段三、UserApp共享视图等按已确认原范围推进；未经实现/验证不要标完成。kube-runtime D是明确可选后续，不能擅自扩大为本轮必做。不要修改Spec缩减原需求。
5. 更新tasks与追加verification；保留历史证据和原基线。审查中任何结论若不成立，给出当前完整调用链和反例测试证据，而非直接删除问题。

## 验证要求

- 每项缺陷补能在修复前暴露错误的行为回归；调用链、物理路径、状态持久化及关键时序都要断言。具体反例见审查文档。
- F01仅用临时目录构造破坏性输入；确保越界拒绝且零业务数据改变。ZIP测试验证实际文件内容和更新。
- 生命周期测试驱动server执行链；K8s写后失败测试到上层control worker，断言保护仍在且后续写被阻止。
- 工具单元/协议测试可用受控替身；真实业务与AI E2E不能伪造响应或降低断言。快照本身变化必须失败，活动源码变化不应污染冻结运行；Python字节码和报告输出不得污染输入。
- 优先cargo nextest run --no-fail-fast，先聚焦，再覆盖受影响默认feature和--all-features；根全量明确加--workspace。app-cli是独立项目，额外fmt/clippy/nextest。文档测试保留cargo test --doc。不要并发Cargo共享target。
- 运行Python工作流、启动器和Make入口回归；随后按影响范围运行Compose test-e2e核心业务，以及远端K8s实际smoke、gateway、userapp、chat。使用.env.local既有配置，保护PVC和共享资源，不把凭据写入文档。
- 远端修复后，核对每份报告实际suite/case、源码与测试快照摘要、镜像digest、部署身份；失败重跑有新run ID及父报告关联，不能覆盖原报告或把局部成功当全套通过。
- 缺LLM/环境故障/中断/缺报告要明确记录，不能skip后称通过。声称既有基线失败必须同条件复现，否则待归因。

## 交付

新增本轮verification，按R01–R08、F01–F05、K01–K05、T01–T07逐项给出：修复状态、关键改动、反例、实际命令/退出码、证据路径和未完成项。将实现完成、组件测试、Compose、K8s部署验收、发布分别报告。说明原需求剩余范围，不能以止血拒绝能力替代实现。正常实现与可逆验证无需逐项请求批准；若真实外部条件阻塞，完成所有独立工作后如实列出。
