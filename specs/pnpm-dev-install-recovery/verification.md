# pnpm 开发依赖安装修复验证记录

日期：2026-09-18。依据：同目录 `spec.md` / `plan.md` / `tasks.md`。

## 基线

| 仓库 | 分支 | 基线 HEAD | 起点状态 |
|---|---|---|---|
| rcoder | feature-userapp | `8adc8db2` fix(runtime): N02 路径护栏三平台统一 + R03 制品激活权归属 owner | 工作区仅含本任务改动；`specs/rcoder-local-cache-per-replica-rbd/`（他组未跟踪目录）未触碰 |
| userapp-workspace-template | main | `0211fd8` refactor: update app-cli commands and documentation for workspace support | 干净 |

工具链：pnpm 12.4.2（宿主 PATH）、pnpm 10.34.5（npx 缓存 shim，事故版本）、node v26.8.2。
事故环境（存档）：容器内 pnpm 10.34.5、项目无 pnpm-lock.yaml。

## 改动清单

### userapp-workspace-template（主修复）

| 文件 | 改动 |
|---|---|
| `cli/src/manifest.ts` | React/Vue 两生成分支 devbuild 改 `--no-frozen-lockfile`（React 保留 `&& pnpm run type-check`） |
| `frontend-react-vite/project.manifest.toml` | `[devbuild]` 同步改 `--no-frozen-lockfile` |
| `frontend-vue3-vite/project.manifest.toml` | 同上 |
| `cli/scripts/test-local.mjs` | 新增断言：init 生成 React、add 生成 Vue 的真实 manifest 含 `--no-frozen-lockfile`、不含裸 `--frozen-lockfile`、React 仍串 type-check |

### rcoder

| 文件 | 改动 |
|---|---|
| `crates/file-server/src/service/pnpm/cli.rs` | 参数构造提取为私有 `install_args`（install_once 调用），默认加 `--no-frozen-lockfile`（位于 extra_args 之前）；新增 3 个参数构造测试。不改 `InstallOptions` 字段、reporter/confirmModulesPurge/超时/自愈逻辑 |
| `crates/file-server-userapp/src/service/userapp/dev_mode.rs` | tests 模块新增走生产路径 `run_dev_builds` 的 6 场景真实 pnpm 回归（本地 `file:` 依赖 fixture，离线） |
| `tests-e2e/tests/compose_userapp_build_rules.rs` | 新增场景 4 `userapp_devbuild_no_lockfile_pnpm_install`（compose 全链路）+ 辅助 `dev_start_to_terminal`/`overwrite_file` |
| `tests-e2e/tools/contracts.py`、`suite_cases.json`、`report_identities.json` | 注册新场景及 10 项验收步骤 |
| `.zcode/plans/pnpm-frozen-lockfile-analysis.md` | 按 plan.md §7 修正（gitignore 内，本地维护） |
| `specs/pnpm-dev-install-recovery/legacy-app-fix.md` | 新增：存量应用定点修复步骤 |

## 实际命令与结果

### A. 模板仓库（cli/）

| 命令 | 退出码 | 结果 |
|---|---|---|
| `npm run build` | 0 | tsc 通过 |
| `npm run pack` | 0 | 7 模板 268 文件重打包（本轮源码） |
| `npm test` | 0 | 39/39 通过，含 4 条新 devbuild 断言（对 init/add 真实生成文件，非 grep 模板源） |

### B. Rust 组件（rcoder）

| 命令 | 退出码 | 结果 |
|---|---|---|
| `cargo nextest run -p file-server install_args --no-fail-fast` | 0 | 3/3 参数构造测试通过 |
| `cargo nextest run -p file-server-userapp devbuild_ --no-fail-fast` | 0 | 6/6 真实 pnpm 链路通过（pnpm 12.4.2） |
| `cargo nextest run -p file-server -p file-server-userapp --no-fail-fast` | 0 | 417/417 |
| 同上 `--all-features` | 0 | 417/417 |
| `cargo fmt --all -- --check` | 0 | 通过（本任务两文件先经 rustfmt --edition 2024 定点格式化，未整仓重排） |
| `cargo clippy -p file-server -p file-server-userapp --all-targets` | 0 | 无告警 |

devbuild 六场景断言要点（生产路径 `run_dev_builds`）：

1. 旧命令反例（`--frozen-lockfile` + 无 lockfile）→ Err 且日志含 `ERR_PNPM_NO_LOCKFILE`、sentinel 检查未执行（app 110 事故形态，永久反例测试）
2. 新命令 + 无 lockfile → 成功、`pnpm-lock.yaml` 生成、后续检查执行
3. 过期 lockfile（package.json 真实新增依赖）→ 安装成功且 lockfile 含新依赖
4. lockfile 已匹配 → 重复安装成功、检查照常
5. 安装失败（`file:./vendor/missing`）→ Err、检查未执行
6. 安装成功但检查脚本 `exit 3` → 仍 Err，不假成功

### C. pnpm 10.34.5（事故版本）回归

shim：`/tmp/pnpm-10.34.5-shim/pnpm` → `exec node ~/.npm/_npx/381139ee5d646d31/node_modules/pnpm/bin/pnpm.cjs "$@"`（本机 npx 缓存中已有 10.34.5，未联网下载；`pnpm --version` 验证输出 `10.34.5`）。

| 命令 | 退出码 | 结果 |
|---|---|---|
| `PATH=/tmp/pnpm-10.34.5-shim:$PATH cargo nextest run -p file-server-userapp devbuild_ --no-fail-fast` | 0 | 6/6 通过（含 ERR_PNPM_NO_LOCKFILE 反例断言） |

### D. Compose 业务验证（既有环境，rcoder 容器 healthy）

| 命令 | 退出码 | 结果 |
|---|---|---|
| `python3 tests-e2e/tools/run.py --suite compose_userapp_build_rules --filter userapp_devbuild_no_lockfile_pnpm_install` | 0 | pass；报告 `tests-e2e/reports/fd0c730c116b4fc2831f53a321f0d7bd/`，13 hard 断言全绿 |
| `python3 tests-e2e/tools/run.py --suite compose_userapp_build_rules` | 0 | 4/4 pass（含新场景）；报告 `tests-e2e/reports/92150e75b515420a89c8d9590b0d6f6d/` |

注：套件首轮全量运行（报告 `599655e3464a4005ada2df32f56ffb3d`）4 场景虽全部
pass，但启动器判定 source changed during run（运行期间编辑了本 specs 文档），
按漂移检查规则作废；随后在无源码改动下重跑得上述有效结果。

新场景证据（JSONL 断言名）：`无 lockfile dev/start completed（--no-frozen-lockfile 安装成功）`、`devbuild 生成 pnpm-lock.yaml（修复前此处 ERR_PNPM_NO_LOCKFILE 失败）`、`dev/list → pid>0（devrun 存活）`、`devrun 服务内容经代理可达（node server.js）`、`frozen + 过期 lockfile → 任务 failed（错误如实传播）`、`dev/stop → Stopped` 全部 OK；verdict=pass。

说明：运行中的 rcoder 容器为 23h 前构建（不含本轮 cli.rs 改动）。该场景链路（`[devbuild]` argv 原样执行）不经过 `pnpm::install` 封装，场景结论不受影响；cli.rs 封装改动由参数构造测试覆盖，其消费者（build_exec / computer packages / ops packages / dev_server start）无冻结相关 extra_args（已核对唯一传参点 `ops/packages.rs:78`）。

## 未完成 / 受阻项

- `make dev-hot` / 镜像热更新被环境操作约束跳过——运行中 rcoder 二进制未包含 cli.rs 改动（影响见上说明）。
- pnpm 10.34.5 为本机 npx 缓存版本，未重新下载校验完整性（缓存条目版本号与目录一致）。
- dev_mode 六场景测试在无 pnpm 的机器上按环境门控跳过并打印原因；验收以本轮真实执行为准。

## 状态区分

| 状态 | 结论 |
|---|---|
| 源码修复完成 | 是（两仓库四处命令、生成器、封装默认值、场景与注册） |
| 测试通过 | 是（模板 CLI 39/39；Rust 417/417 默认+全 feature、clippy/fmt 干净；pnpm 10.34.5 与 12.4.2 双版本六场景；compose 新场景及套件见 D） |
| 发布生效 | 否——模板 CLI 需 `npm run release`（或 beta）发 npm + tag 推送、沙箱预装版本升级；Rust 侧需重建含新版 file-server 的 dev/生产镜像。本任务未执行任何发布动作 |
| 存量应用恢复 | 否——app 110 等需按 `legacy-app-fix.md` 在目标环境定点修复（本任务不改远端工作区） |
