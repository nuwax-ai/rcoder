# pnpm 构建脚本审批策略 (file-server)

> **状态（2026-08-07）**：当前镜像 pnpm 钉在 `pnpm@10`；file-server 四条 install 路径已按本文策略实现并运行正常。是否统一升级到 pnpm 11（或后续稳定版）待后续评估，**升级时需配合调整 dev_server/build 两条路径（见 §6.2）**。
>
> **目标读者**：维护 file-server 的 `pnpm install` 流程，或负责 pnpm 版本升级的同学。
> **关联代码**：`crates/file-server/src/service/pnpm_config.rs`、`service/pnpm/cli.rs`、`handlers/computer/exec.rs`、`handlers/build.rs`、`service/dev_server/mod.rs`。

---

## 1. 背景：pnpm 为什么会"拦"构建脚本

从 pnpm 10 起，出于供应链安全，**依赖里的生命周期脚本（`preinstall` / `install` / `postinstall` 等）默认不执行**，必须显式批准才跑。这是为了防恶意 npm 包借 `postinstall` 在装包时偷偷执行命令。

pnpm 用一个 `allowBuild(depPath)` 回调逐个判定每个依赖的脚本能否执行（pnpm 源码 `building/during-install/src/index.ts`）：

| 回调返回 | 行为 |
|---|---|
| `true` | 执行构建脚本 |
| `false` | 显式禁止，**静默跳过**（不报错、不警告） |
| `undefined`（不在任何清单里） | 跳过，并计入 `ignoredBuilds` |

`ignoredBuilds` 非空时怎么处理，取决于 `strictDepBuilds`：

- **pnpm 10**：`strictDepBuilds` 默认 `false` → 只打黄框警告「Run `pnpm approve-builds`」，install 仍成功（exit 0），但被跳过脚本的依赖（esbuild / sharp / @swc/core 等）在运行时是残废的。
- **pnpm 11**：`strictDepBuilds` 默认翻成 `true` → 直接 `IgnoredBuildsError`，**install 以非零退出码失败**。

> 这就是容器/CI（无 TTY、没法交互批准）在 pnpm 11 下 `pnpm install` 直接挂掉的根因。file-server 是**非交互**调用 pnpm（stdin = null），必须自己把放行策略配好，不能依赖交互式 `pnpm approve-builds`。

---

## 2. pnpm 的三种策略 + 互斥规则

pnpm 10 用**三种互斥的策略形态**决定"哪些依赖能跑构建脚本"：

| 策略 | 配置键 | 语义 | 设法 |
|---|---|---|---|
| 允许清单 | `onlyBuiltDependencies` | 只有列出的能跑 | package.json `pnpm.*` / pnpm-workspace.yaml / `.npmrc`(kebab) |
| 禁止清单 | `neverBuiltDependencies` | 列出的绝不跑，其余按默认 | 同上 |
| 全部允许 | `dangerouslyAllowAllBuilds=true` | 无条件全跑（= pnpm 9 行为） | `.npmrc` kebab / CLI `--config.dangerouslyAllowAllBuilds=true` |

**互斥规则**：pnpm 10 禁止混用这些形态。Issue [pnpm#8935](https://github.com/pnpm/pnpm/issues/8935) 的报错原文：

```
ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES
Cannot have both neverBuiltDependencies and onlyBuiltDependencies
```

`dangerouslyAllowAllBuilds=true`（等价于"清单 = 全部"）一旦遇到任何 `neverBuiltDependencies`，同样构成"全部允许 vs 禁止这些"的矛盾，触发同一个错误。

**为什么不静默二选一**：遇到一个在 neverBuilt 名单里的包，pnpm 没法替你决定到底跑不跑。与其悄悄选一个导致行为与预期不符，不如直接报错逼你收敛成单一策略。这是**故意的护栏**，不是 bug。

---

## 3. file-server 的四条 install 路径分流

file-server 有四处会跑 `pnpm install`，**按是否需要原生构建脚本、是否能容忍冲突，分流处理**：

| 路径 | 入口 | `dangerouslyAllowAllBuilds` | 怎么给的 | 调 `ensure_pnpm_install_config`？ |
|---|---|:---:|---|:---:|
| **install-project** | `handlers/computer/exec.rs:241` | ✅ | CLI `--config.dangerouslyAllowAllBuilds=true`（`exec.rs:238`） | ✅（`exec.rs:230`） |
| **build-agent-package** | `handlers/computer/exec.rs:305` | ✅ | `ensure_pnpm_install_config` 写进项目 `.npmrc` | ✅（`exec.rs:303`） |
| **dev_server（vite 运行）** | `service/dev_server/mod.rs:217` | ❌ **故意不开** | 只 `create_pnpm_npmrc` + `sanitize` | ❌（见 `mod.rs:453-461` 注释） |
| **build（vite 编译）** | `handlers/build.rs:493` | ❌ **故意不开** | 默认 `InstallOptions::prefer_offline()` | ❌ |

统一的 CLI 兜底（**所有**路径都带，见 `service/pnpm/cli.rs:37`）：

- `--config.confirmModulesPurge=false` —— file-server 非 TTY（stdin null）spawn pnpm，当 `node_modules` 与 lockfile 不一致时 pnpm 要交互确认是否 purge，无 TTY 则 abort（`ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY`），install 永久失败、vite 起不来。此 flag 跳过确认。详见 `cli.rs:29-37` 注释。

---

## 4. 为什么 dev_server / build 故意不开 allow-all

### 4.1 冲突：开 allow-all 会撞 pnpm 内置 neverBuilt

`dev_server/mod.rs:453-461` 的 `write_npmrc` 注释原话：

> 不调 `ensure_pnpm_install_config`：其 append 会加 `dangerously-allow-all-builds=true`，在 **pnpm 10.x 下与内置 `neverBuiltDependencies` 冲突**（`ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES`），而 vite dev 的依赖（esbuild）走可选依赖机制、不需 build 脚本。NO_TTY 由 pnpm cli 的 `--config.confirmModulesPurge=false` 兜底。

机制（见 §2）：`dangerouslyAllowAllBuilds=true` ⇒ 隐式"允许清单 = 全部" ⇒ 与 pnpm 内置/默认的 `neverBuiltDependencies`（禁止清单）互斥 ⇒ 报错。

关键点：`sanitize_pnpm_built_dependencies_config`（`pnpm_config.rs:126`）**只能清掉用户项目里设的** `never/only/ignoredBuiltDependencies`（package.json + pnpm-workspace.yaml + .npmrc），**清不掉 pnpm 内置的那份**。所以在 pnpm 10.x 下，开 allow-all 始终有撞内置 neverBuilt 的风险——这正是 dev_server/build 选择「整个不碰 allow-all」的原因。

### 4.2 vite 生态不需要构建脚本：esbuild 走 optionalDependencies

dev_server/build 能安全地不开 allow-all，是因为 **vite 生态的原生依赖根本不需要 `postinstall`**。

以 esbuild 为例，包结构是：

- 主包 `esbuild`：JS API + 薄 bin shim
- `optionalDependencies`：`@esbuild/linux-x64`、`@esbuild/darwin-arm64`、`@esbuild/win32-x64` …… 每个平台一个

pnpm 装包时按当前 `os`/`cpu` 只装匹配的那个 optional 子包（预编译好的二进制），主包在**运行时**用 `process.platform`/`process.arch` 算出子包名再 `require()` 进来。**整个过程没有 `postinstall`**——二进制是作为普通依赖被解包出来的，不是装完再下载/编译。

→ 典型 vite 项目的依赖里没有"需要被批准的构建脚本" → pnpm 审批门对它完全不生效 → 不设任何策略也不会被 `strictDepBuilds` 拦，更不会冲突。**不碰 allow-all 是最干净的方案。**

> **对比**：`bcrypt`、`better-sqlite3`、老版 `node-sass` 用 node-gyp 在 `postinstall` 现场编译；老版 `sharp` 在 `postinstall` 下载 libvips——**这些才需要构建脚本**。所以通用安装路径（install-project / build-agent-package）必须开 allow-all。

### 4.3 install-project / build-agent-package 为什么必须开

这两条是**通用安装/打包**路径，装的可能是任意项目。万一里面有 `bcrypt`/`better-sqlite3` 这类依赖 `postinstall` 的包，不开 allow-all 它们的脚本就被跳过 → 装出来的依赖残废，运行时才崩。所以必须让脚本跑。

它们靠 `ensure_pnpm_install_config`（`pnpm_config.rs:42`）规避冲突，三步（均 best-effort，失败只 warn 不阻断 install）：

1. `create_pnpm_npmrc`：写 `.npmrc` 模板（`package-import-method=copy` 等，避免 JuiceFS/FUSE hardlink 失败）。
2. `sanitize_pnpm_built_dependencies_config`：**先**清掉项目自设的 `never/only/ignoredBuiltDependencies`，减少冲突面。
3. `append_install_lines`：缺失才补 `dangerously-allow-all-builds=true` / `production=false` / `confirm-modules-purge=false`。

> install-project 还额外在 CLI 上带 `--config.dangerouslyAllowAllBuilds=true`（`.npmrc` 的双保险）；build-agent-package 走默认 `InstallOptions`，靠 `.npmrc` 兜底。

---

## 5. pnpm 11 如何根除这一类冲突

pnpm 11 直接**删掉了 `neverBuiltDependencies` / `onlyBuiltDependencies` / `ignoredBuiltDependencies`**（[11.0 release blog](https://pnpm.io/blog/releases/11.0)），统一换成单个 `allowBuilds` map（`包名: true|false`）。

已在 pnpm 源码核实（`pnpm11/` 树）：

- 搜 `ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES` → **零命中**，这个错误码在 pnpm 11 已不存在。
- `neverBuiltDependencies` 不再作为有效配置键（pnpm 12 的 napi 层 `reject_non_empty_list` 直接拒绝非空 `neverBuiltDependencies`）。

策略模型从"三种互斥形态"变成"一个 map + `strictDepBuilds`"：**互斥的根源消失 → 冲突这一整类错误在 pnpm 11 不复存在**。

但注意 pnpm 11 的代价：`strictDepBuilds` 默认 `true`，**任何**被忽略的构建脚本都会让 install 失败。所以一旦升级：

- dev_server/build 两条路径如果装的 vite 项目里混进了需要 `postinstall` 的依赖（用户加了 `sharp`/`bcrypt` 等），不开 allow-all 就会 install 失败。
- → 升级时必须把这两条路径也接上 `dangerouslyAllowAllBuilds=true`（pnpm 11 下已无冲突，安全）。

---

## 6. 当前决策与未来升级路径

### 6.1 现状：留在 pnpm 10

- 镜像 `pnpm@10`，file-server 四路径策略正确，运行正常。
- **不要**在 pnpm 10 下给 dev_server/build 加 `dangerouslyAllowAllBuilds=true`——会重新引入 §4.1 的冲突。

### 6.2 升级 pnpm 11（或后续稳定版）时的 checklist

升级与"统一四条路径"是**同一个动作**，不能拆开先做：

1. **镜像层**（`build-agent-docker` 仓库 `build_config/rcoder/Dockerfile.base`）：
   - `npm install -g pnpm@10` → `pnpm@11`（或目标稳定版）。
   - 删掉误导的 `ENV PNPM_ENABLE_PRE_POST_SCRIPTS=true`——那是管"项目自身 npm script 的 pre/post 伴随钩子"的 `enable-pre-post-scripts`，跟依赖构建放行是两套机制，设了也没用。
2. **file-server dev_server 路径**（`dev_server/mod.rs:453-461` `write_npmrc`）：
   - 改为调用 `ensure_pnpm_install_config`（即恢复 append `dangerously-allow-all-builds=true`），或在 `mod.rs:217` 的 `pnpm::install` 加 `--config.dangerouslyAllowAllBuilds=true`。
   - pnpm 11 下 neverBuilt 已删，§4.1 的冲突不再成立。
3. **file-server build 路径**（`handlers/build.rs:493`）：
   - 同理补上 allow-all（CLI arg 或 `ensure_pnpm_install_config`）。
4. **install-project / build-agent-package**：无需改动，已开 allow-all；pnpm 11 下 `sanitize` 步骤变成 no-op（旧键已不存在），但保留无害。
5. **回归验证**：用一个含 `postinstall` 依赖的项目（如带 `better-sqlite3` 或老 `sharp`）分别走 dev_server、build、install-project 三条路径，确认 install 不再因 `strictDepBuilds` 失败、且原生模块在运行时可用。
6. **可选简化**：pnpm 11 下 `onlyBuiltDependencies`/`neverBuiltDependencies` 已删，`sanitize_pnpm_built_dependencies_config` 可评估是否退役（保留作向前兼容也行）。

### 6.3 不推荐的替代方案

- `pnpm approve-builds --all`：作用于**特定项目**的 `pnpm-workspace.yaml`，file-server 动态装任意项目，每装一个就要跑一次，容器里不现实。
- 环境变量：`dangerously-allow-all-builds` 带连字符，shell 无法 `export`（`PNPM_CONFIG_DANGEROUSLY-ALLOW-ALL-BUILDS` 非法标识符），这条路堵死。
- `--ignore-scripts`：方向相反——跳过所有脚本（含项目自身的），会让需要构建的依赖直接残废。

---

## 7. 相关代码位置（速查）

| 关注点 | 位置 |
|---|---|
| install 前配置准备入口（写 .npmrc + sanitize + append allow-all） | `service/pnpm_config.rs:42` `ensure_pnpm_install_config` |
| append `dangerously-allow-all-builds=true` 的逻辑 | `service/pnpm_config.rs:253` `append_install_lines` |
| 清理互斥 built-deps 键 | `service/pnpm_config.rs:126` `sanitize_pnpm_built_dependencies_config` |
| 互斥键常量 | `service/pnpm_config.rs:23-34` |
| pnpm install 的 CLI 参数组装（含 `confirmModulesPurge=false`） | `service/pnpm/cli.rs:25-38` |
| install-project 路径 | `handlers/computer/exec.rs:184`（handler）/ `:230`（配置）/ `:241`（install） |
| build-agent-package 路径 | `handlers/computer/exec.rs:287`（handler）/ `:303`（配置）/ `:305`（install） |
| dev_server 路径 + 不开 allow-all 的注释 | `service/dev_server/mod.rs:217`（install）/ `:453-461`（`write_npmrc` 注释） |
| build 路径 | `handlers/build.rs:493`（install） |

---

## 8. 参考

- [pnpm settings — build](https://pnpm.io/settings/build) — `dangerouslyAllowAllBuilds`（v10.9.0）/ `strictDepBuilds`（v10.3.0，v11 默认 true）/ `allowBuilds` 定义与默认值
- [pnpm approve-builds CLI](https://pnpm.io/cli/approve-builds) — `--all` 非交互批准、位置参数
- [pnpm 11.0 release blog](https://pnpm.io/blog/releases/11.0) — `onlyBuiltDependencies`/`neverBuiltDependencies` 删除，统一为 `allowBuilds`；`strictDepBuilds` 默认 true
- [pnpm Issue #8935](https://github.com/pnpm/pnpm/issues/8935) — `ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES: Cannot have both neverBuiltDependencies and onlyBuiltDependencies`
- 镜像层 pnpm 安装与 `PNPM_ENABLE_PRE_POST_SCRIPTS` 误配：`build-agent-docker` 仓库 `build_config/rcoder/Dockerfile.base:80,84`
