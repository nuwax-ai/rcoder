# UserApp 运行态所有权统一：实施交接

日期：2026-09-15。状态：**方案完成，代码未实施，运行验收未执行**。

本次任务只新增此目录文档，不修改业务代码，不提交、发布或操作环境。

## 阅读顺序

1. [spec.md](spec.md)：需求、范围、不变量与失败语义。
2. [plan.md](plan.md)：源码证据、架构、协议、阶段设计与迁移方式。
3. [tasks.md](tasks.md)：执行顺序、完成标准、测试矩阵与交付记录。
4. [agent-prompt.md](agent-prompt.md)：可复制给另一开发 agent 的完整提示词。

2026-09-17 后续补充：三平台要求见 [cross-platform.md](cross-platform.md)；app-cli 与 file-server-proxy 的无容器、自包含宿主机要求见 [原生运行时文档](../native-desktop-runtime/README.md)。Electron 仅作为后续使用场景，不纳入客户端开发。继续开发采用[统一提示词](../development-review-2026-09-17/claude-prompt.md)，并核对审查发现及实际 verification；本文件开头状态及下面基线保留为首次方案的历史记录。

## 核心决策

- 阶段一独立修复启动错误传播，不等待架构切换。
- 最终由一个 `app-cli serve` 负责运行态：启停、重启、目录激活、服务编排、恢复。
- file-server 保留构建和任务/SSE 适配，停止直接修改有效运行目录或启动第二套编排。
- 开发源码模式与制品模式共享控制机制，保留不同命令和目录语义。
- 不按端口抢杀进程，不因 API 超时回退到 legacy spawn，不凭健康旧应用判断新操作成功。
- 运行操作结果、用户期望状态、实际健康是三个维度，不互相冒充。

## 基线和并行工作

仓库：`/Users/soddy/Documents/git-workspace/rcoder`。

核对时 HEAD：`a4380b37a77d54797473c0417f70f03e66480b71`；app-cli 版本：`0.3.5`。

**以上是阅读基线，不是已验收镜像身份。** 工作树已有大量未提交改动，涉及 app_manager、docker_manager、shared_types、UserApp builder、Custom Page 预览、远端 K8s 和 E2E。不得 reset、覆盖或整仓暂存；开发时重新读取 status/diff。不能假定新 worktree 的 HEAD 包含这些未提交依赖。若做隔离 checkout，应仅带入明确需要、已核对的改动并记录其来源。

推荐开发分支名：`codex/userapp-runtime-ownership`，由开发 agent 在实际工作树状态允许时创建；本文档没有创建该分支。

## 相关约束

- 根目录 [AGENTS.md](../../AGENTS.md)。
- [并发修复规范](../concurrency-quality-repair/spec.md)、[计划](../concurrency-quality-repair/plan.md)、[任务](../concurrency-quality-repair/tasks.md)：继承 D01–D06 的持久化、身份和恢复不变量；历史任务的专属环境授权不能跨任务照搬。
- [UserApp 生命周期](../userapp-lifecycle-convergence/spec.md)、[计划](../userapp-lifecycle-convergence/plan.md)：停止意图、取消、恢复保护和操作归属。
- [Custom Page 规范](../custom-page-preview-routing/spec.md)：该领域不是本次 UserApp dev 改造对象，尤其不能将 PG/K8s 依赖引入独立 file-server/npm 形态。
- [远端 K8s 使用说明](../remote-k8s-dev/README.md)、[E2E 启动器说明](../../tests-e2e/tools/README.md)。

本方案规划三阶段完整实现。阶段一必须形成可独立验证和交付的改动，不与后两阶段混为一个无法单独验收的大变更。开发交接不自动授权修改既有用户应用、自建 supervisor 配置或生产部署。
