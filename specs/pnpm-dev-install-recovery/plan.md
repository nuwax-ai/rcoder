# 技术实施方案

先读同目录 spec.md 与 tasks.md。只读背景：../../.zcode/plans/pnpm-frozen-lockfile-analysis.md。

## 1. 真实链路与修复位置

源码位置以当前检出为准，以下是本轮核对位置：

| 文件 | 位置/职责 |
|---|---|
| rcoder/crates/file-server-userapp/src/handlers/userapp_dev_server.rs | 约353行调用 run_dev_builds；约476行后进入 start_dev |
| rcoder/crates/file-server-userapp/src/service/userapp/dev_mode.rs | 约76行选择 devbuild argv；130行执行；156行包装 dev build failed |
| rcoder/crates/file-server/src/service/dev_server/start.rs | 约100行 manifest 分叉；普通 Vite 路径才调用 pnpm 安装封装 |
| userapp-workspace-template/cli/src/manifest.ts | 约404、413行生成 React/Vue devbuild |
| userapp-workspace-template/cli/scripts/pack-templates.mjs | 约263行排除模板源 project.manifest.toml |
| rcoder/crates/file-server/src/service/pnpm/cli.rs | install_once 构造实际安装参数，不是 run_install |

## 2. 主修复：生成器与模板同时修改

仓库：/Users/soddy/Documents/git-workspace/userapp-workspace-template。

必须同步三份文件中的四处命令：
- cli/src/manifest.ts：React 与 Vue 两个生成分支。
- frontend-react-vite/project.manifest.toml。
- frontend-vue3-vite/project.manifest.toml。

React 目标：

```toml
[devbuild]
command = ["sh", "-c", "pnpm install --no-frozen-lockfile && pnpm run type-check"]
```

Vue 目标：

```toml
[devbuild]
command = ["pnpm", "install", "--no-frozen-lockfile"]
```

保留全部其他字段、devrun、生产 build 命令和产物布局。生成器仍是已有生成规则的位置，不为四处字符串新增复杂配置层。

补充模板 CLI 的实际生成测试：构建并打包后，经 init/add 生成 React/Vue 服务，读取产物 manifest，断言安装选项正确，React 的 && pnpm run type-check 仍存在。核实 release 等其他共用生成器入口是否覆盖；同一生成函数复用不要求重复堆砌同义测试。

## 3. 附带加固：通用 file-server pnpm 安装默认值

仓库：/Users/soddy/Documents/git-workspace/rcoder。

在 crates/file-server/src/service/pnpm/cli.rs 的 install_once 实际参数构造中增加 --no-frozen-lockfile。

- 本次默认策略允许更新 lockfile，不改变 reporter、prefer-offline、confirmModulesPurge、开发依赖和超时处理。
- 将现有参数构造提取为同文件私有纯函数（如 install_args），install_once 必须调用该函数；测试同一函数，不在测试里复制一个无关 args 列表。
- 无需新增 InstallOptions 字段。
- 建议把默认选项放在 extra_args 之前，保持已有显式调用参数的覆盖约定；检查现有调用者是否传冻结相关参数。若新增冲突用例，必须核对目标 pnpm 的实际参数优先级，不能只凭 Vec 顺序宣称行为成立。
- 不将这一层的参数自动注入用户 manifest 的 shell 命令。
- 它还被包安装、构建依赖安装等入口复用，审查这些消费者；不要把影响范围描述成仅一个 Vite 启动入口。

注释解释业务策略即可：开发安装允许 lockfile 创建/更新。不要写成“stdin=null 必然触发 CI”。

## 4. 存量应用处置

模板包升级不更新已有应用的 manifest。对 app 110 或其他明确受影响的应用：

1. 确认应用身份、持久源码 workspace 和实际失败命令；不能根据 Pod 名或端口就决定修改对象。
2. 记录原文件或保存小范围 diff，在没有并发构建/编辑时修改对应 [devbuild] 参数。
3. 保留自定义前后置命令及 type-check。只处理经核对的冻结安装片段，不全文件/全应用搜索替换，不删除 lockfile、源码或 PVC。
4. 修改写入持久源码并通过现有源码保存/提交机制保存；只改临时目录不算完成。
5. 重新执行开发构建，检查 lockfile 生成、后续检查执行及任务结果；成功后再验证启动。

本轮开发交付必须提供这些操作步骤；实际修改远端应用在明确目标环境和允许操作后执行。不以重建应用作为常规修复办法，不引入后台批量迁移。

## 5. 有效验证

### A. 模板生成与打包

在模板仓库 cli/ 执行既有 build、pack、test 入口，新增断言接入正式测试入口：

```bash
npm run build
npm run pack
npm test
```

必须检查 CLI 实际生成的文件，不能只 grep 模板源。确认打包后的 dist/templates 使用本轮源码。

### B. UserApp manifest 执行链

在 file-server-userapp 的现有测试结构中增加针对 run_dev_builds 的测试，调用生产路径。选 pnpm 10.34.5 作为事故回归版本；另测实际部署版本若不同，并记录准确版本。

使用临时 workspace、小型本地依赖 fixture 和受控检查脚本，避免下载完整 React/Vue 依赖造成网络噪声；fixture 是构建协议测试，不宣称为完整模板或业务 E2E。

| 场景 | 必须断言 |
|---|---|
| 旧 manifest + 无 lockfile | 相同 fixture 在修复前失败，后续检查未执行 |
| 新 manifest + 无 lockfile | 安装成功、生成有效 lockfile、后续检查确实执行 |
| 已有过期 lockfile | 修改依赖声明后安装可同步 lockfile；不能用仅改无关字段伪造过期场景 |
| lockfile 已匹配 | 重复安装成功，后续检查照常执行 |
| 安装失败 | 例如不存在的本地 file: 依赖；任务失败且后续检查未执行 |
| React 后续检查失败 | 安装成功但检查脚本返回非零；run_dev_builds 仍失败，不返回假成功 |

真实 pnpm 用例的命令必须可单独明确执行。缺少指定工具时报告前置失败，不把跳过计为验收。不要为测试设置生产环境全局变量或修改开发者的 pnpm 配置。

### C. 通用封装与回归

参数测试覆盖 default/prefer_offline/extra_args，验证默认 no-frozen 参数及既有参数保留。

```bash
cargo nextest run -p file-server -p file-server-userapp --no-fail-fast
cargo nextest run -p file-server -p file-server-userapp --no-fail-fast --all-features
cargo fmt --all -- --check
cargo clippy -p file-server -p file-server-userapp --all-targets
```

先聚焦新增反例，再执行上述受影响回归；检查 feature 是否有实际差异，避免无意义重复。已有并发改动与失败须归因，不能为“清零”顺手修改无关模块。格式化限定本次文件，避免覆盖并发工作。

### D. 实际业务验证

组件测试通过后，在既有 Compose 流程验证无 lockfile 的 UserApp 开发构建/启动。真实模板验证应使用实际生成的 React/Vue 项目，检查对应构建任务与后续服务可用。

若可操作事故测试环境，再对定点修复的存量应用验证；否则明确记录尚未恢复线上 app 110。不新增 K8s 专属实现，不以 K8s smoke 替代业务验证。

## 6. 分发边界

- 模板：源码修改 → template-cli 构建/打包 → 按既有流程发布新版本 → 更新沙箱实际使用的预装版本。更新技能/提示词只限相关安装说明。
- Rust 通用封装：需要重建包含新版 file-server/file-server-proxy 的镜像或包；模板发布不包含这部分 Rust 改动。
- build-agent-docker 的 agent-runner/app-runtime 运行期间使用预装工具，不添加启动时 npm install。
- 不强制本轮升级 app-cli：主修复并不修改 app-cli，其发版任务与本任务分开判断。
- 实施阶段完成代码和验证后记录所需发布步骤；未经本任务进一步发布指令，不执行 npm publish、tag、镜像推送、生产部署或远端存量应用修改。

## 7. 事故文档修正

更新原分析文档：修正 handler → run_dev_builds → 启动阶段的顺序；新增生成器与 pack 排除证据；Vue 路径改为 frontend-vue3-vite/project.manifest.toml；移除 stdin 对照的矛盾表述；将“重建应用”替换为持久源码定点修复。保留历史假设已被纠正的说明，不伪造本轮容器实测证据。
