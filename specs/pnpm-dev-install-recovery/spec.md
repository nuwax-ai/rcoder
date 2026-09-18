# UserApp 开发依赖安装：允许生成和更新 lockfile

日期：2026-09-18。状态：已实施（验证见 verification.md）。本规范优先于事故分析初版的 stdin/CI 假设和“只改 cli.rs”范围。

## 目标

1. 从 React/Vue 模板新建或追加的服务，无 pnpm-lock.yaml 时可完成开发依赖安装并生成 lockfile；package.json 更新后允许同步更新 lockfile。
2. 保留 React 安装后的 type-check；安装或检查失败必须如实终止该服务 devbuild。
3. 通用 file-server pnpm 安装封装默认允许生成和更新 lockfile。该加固是另一条调用链，不冒充本次 UserApp 故障主修复。
4. 为已有应用提供定点、持久、可核验的源码修复办法，不要求删除或重建应用。

## 已确认的源码事实

- UserApp dev handler 先调用 run_dev_builds；构建成功后才进入 dev_server.start_dev/start_dev_manifest。
- run_dev_builds 直接执行 manifest 的开发构建 argv，不经过 file-server 的 pnpm::install 封装。
- React/Vue 模板及 template-cli 生成器都写有显式 --frozen-lockfile。
- pack-templates.mjs 排除 project.manifest.toml；CLI 生成器才是 init/add 等生成文件的来源。只改模板 TOML 不能修复脚手架生成结果。
- 事故文档记录 app 110 的实际 manifest 含显式冻结参数；本轮仅核对源码，未独立重跑该容器事故。

## 范围与非目标

涉及 rcoder 与 userapp-workspace-template；build-agent-docker 只核查后续分发入口，确有需要再定点修改。

不改变生产 [build]、发布安装或用户自定义构建命令的策略；不全仓替换 frozen-lockfile；不改 stdin、不伪造 CI 环境、不删除现有 lockfile、不吞错误、不跳过 type-check。

不引入通用 shell 命令重写器、全量应用自动迁移或新的安装配置体系。无需统一添加 Cargo --locked，也不开展依赖版本锁定改造。

## 完成标准

生成器、模板及运行链测试通过，失败仍可靠传播；明确区分代码完成、模板包分发、镜像更新与存量应用修复，任何未执行步骤单独列出。
