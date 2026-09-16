请在 `/Users/soddy/Documents/git-workspace/rcoder` 实施 remote-k8s 开发验证体验优化。

先阅读：
1. `AGENTS.md`
2. `specs/remote-k8s-dev-optimization/spec.md`
3. `specs/remote-k8s-dev-optimization/plan.md`
4. `specs/remote-k8s-dev-optimization/tasks.md`
5. `specs/remote-k8s-dev/README.md` 及 `gateway-repair-2026-09-16.md`

当前目标是让 Mac 上未提交的代码也能方便地同步到个人 Linux K8s 集群，构建部署并验证；测试期间继续编辑不影响正在运行的快照，重复验证少做无效工作。

按方案完成必做项：
- 固定测试快照，verify 的构建和测试使用同轮输入；显式保存服务端/测试源码身份。不要只删除源码漂移断言。
- 实现安全构建复用，从保守缓存逐步优化目标级缓存；保持对旧二进制误复用的防护，不能直接删全源touch。
- Chat独立场景筛选和失败重跑；UserApp保持有依赖的完整生命周期链，未建立独立fixture前不允许任意截断。
- status/check及完整诊断，分清历史结果、实时状态和业务验收；保留环境锁、资源所有权、部署UID/generation/digest保护。
- 更新文档、完成聚焦回归和真实集群验证；watch属于后续可选项，不阻塞必做部分。

工作要求：
- 先看git status与相关diff，保留现有业务开发修改，只修改此任务必需文件。此前未验证的工具试改已经撤回，不应假定有可直接复用的实现。
- 继续使用现有Python工具和Make入口，不迁移语言，不绕过严格启动器；不要求用户提交Git，不改主仓库HEAD/index，不自动提交、推送或发布。
- 复用`.env.local`中的远端连接、namespace、registry和地址，不回显凭据，不写死个人IP。测试集群目前Gateway已恢复，worker的Tailscale已卸载；不要重装网络组件。
- 测试快照校验必须捕获新增输入、删除、内容和链接变化；报告/缓存不能污染输入指纹。避免依赖活动仓库对象库的shared clone来声称永久独立快照。
- 新Make参数安全传递，禁止不受信任参数经shell替换执行。
- Rust测试优先nextest。工作流改造重点运行Python回归、启动器回归、Make参数测试与真实K8s场景；跨Compose共享启动器变更需补对应回归。没有业务修改则不无理由运行全workspace测试。
- 从第一批开始记录耗时和身份。相同输入cache hit、输入变化cache miss、测试时活动目录继续编辑、冻结目录篡改被拒绝、部署被替换被拒绝，都要有证据。
- 真实Chat必须用真实LLM；缺少配置记录阻塞，不能mock或跳过后算通过。UserApp/Chat若暴露业务缺陷，保留报告并单列，不擅自修改业务语义或放松断言。
- 按A/B/C批次推进；tasks只勾实际完成并验证的项目。保守缓存完成不能代替细粒度缓存完成，status不能代替check，局部场景通过不能代替全套通过。

无需为常规实现步骤再次请求确认。最终提供：实现范围、改动文件、新旧命令使用方法、测试命令和退出码、真实集群报告、构建前后耗时、未完成项及明确原因。把证据写入 `specs/remote-k8s-dev-optimization/verification.md`。
