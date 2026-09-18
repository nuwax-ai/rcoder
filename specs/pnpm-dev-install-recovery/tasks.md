# Qoder 执行清单

- [x] 阅读各仓库规则和本目录 spec.md、plan.md；记录基线与无关改动。
- [x] 补模板生成的反例，确认旧生成结果含冻结参数。（旧生成器源码 manifest.ts:404/:413 修改前直接核读；"旧命令失败"反例以持久测试落地：dev_mode `devbuild_frozen_without_lockfile_fails_and_skips_check` + E2E 场景 4 frozen 阶段）
- [x] 修改 manifest.ts 两分支及 React/Vue 两份模板。
- [x] 跑 build/pack/test，核验真实生成文件。（39/39，含 init/add 真实产物 manifest 断言）
- [x] 提取同文件 pnpm 参数构造 helper，加默认 --no-frozen-lockfile，补参数测试；保持 extra_args 契约。（install_args 3 测试；唯一 extra_args 调用点 ops/packages.rs:78 无冻结参数）
- [x] 补 file-server-userapp 真正执行 manifest 的回归，记录旧命令失败、新命令通过。（六场景走生产路径 run_dev_builds；pnpm 12.4.2 与 10.34.5 双版本）
- [x] 覆盖过期 lockfile、安装失败、后续检查失败和重复执行。
- [x] 完成受影响 Rust 检查与 Compose 业务验证；不可用时记录准确阻塞项。（nextest 417/417 默认+全 feature、clippy/fmt 干净；Compose 新场景 13 断言全绿 + build_rules 套件；dev-hot 环境刷新受操作约束跳过，已记录）
- [x] 修正原事故文档，写存量应用定点处置和分发步骤。（.zcode 分析文档 + legacy-app-fix.md）
- [x] 创建 verification.md，记录每仓库基线、命令/退出码、证据、未运行项。
- [ ] 按仓库精确暂存并提交本任务文件或改动块；不 git add -A，不 push、不发布、不部署。

未完成或受阻的项目保持未勾选。源码修复不等于存量 app 110 已恢复，也不等于预装模板 CLI 已升级。
