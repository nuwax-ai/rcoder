# RCoder CLI npm Packages

This directory contains npm package templates for distributing rcoder-cli binaries.

## Package Structure

```
npm/
├── rcoder-cli/                    # Main package (platform detection + wrapper)
│   ├── package.json
│   └── bin/cli.js
├── rcoder-cli-linux-x64/          # Linux x86_64 binary
├── rcoder-cli-linux-arm64/        # Linux ARM64 binary
├── rcoder-cli-darwin-x64/         # macOS x86_64 binary
├── rcoder-cli-darwin-arm64/       # macOS ARM64 binary
└── rcoder-cli-win32-x64/          # Windows x86_64 binary
```

## Installation

### Stable version
```bash
npm install -g rcoder-cli
```

### Beta version
```bash
npm install -g rcoder-cli@beta
```

### Using China mirror
```bash
npm config set registry https://registry.npmmirror.com
npm install -g rcoder-cli
```

## CI/CD

Two GitHub Actions workflows handle publishing:

- `release.yml` - Publishes stable versions with `@latest` tag
  - Triggered by tags like `v1.0.0` or `1.0.0`
- `release-beta.yml` - Publishes beta versions with `@beta` tag
  - Triggered by tags like `v1.0.0-beta.1` or `1.0.0-beta.1`

## How it works

1. User runs `npm install -g rcoder-cli`
2. npm installs main package + optional dependency for current platform
3. When user runs `rcoder-cli`, `bin/cli.js` detects platform and executes the binary from the platform package

## Version Management

All packages share the same version number. The CI automatically updates versions from the git tag before publishing.

---

# @nuwax-ai/app-cli Packages

跨平台 UserApp 构建与服务管理器的 npm 分发，支持宿主机和容器中的服务编排、Pingap 路由及管理 API。

## Package Structure

```
npm/
├── app-cli/                      # 主包（平台探测 wrapper，bin: app-cli）
│   ├── package.json
│   └── bin/cli.js
├── app-cli-linux-x64/            # Linux x86_64 二进制
├── app-cli-linux-arm64/          # Linux ARM64 二进制
├── app-cli-darwin-x64/           # macOS x86_64 二进制
├── app-cli-darwin-arm64/         # macOS ARM64 二进制
└── app-cli-windows-x64/          # Windows x86_64 二进制
```

## Installation

```bash
npm install -g @nuwax-ai/app-cli
app-cli --help
```

典型用法：UserApp workspace 模板的本地验证三步闭环——

```bash
app-cli gen-lock --workspace <workspace>          # manifest 校验 + release.lock.toml
app-cli build --workspace <workspace> --deploy-dir <dir>        # 逐服务编译 + 产物态部署布局（或 --dev 三分派）
app-cli serve --workspace <dir>         # 产物态运行（pingap :9080 / admin :3010）
```

### 如何选择子命令

| 命令 | 用途 |
|---|---|
| `serve` | 常驻服务管理器，统一处理启动、停止、重启与部署；平台和长期运行优先使用 |
| `run` | 前台运行当前工作区服务，本地编排时持续监督到停止或退出；不执行构建 |
| `build` | 构建服务，可生成部署目录；不启动服务 |
| `gen-lock` | 校验 manifest 并生成 `release.lock.toml`；不构建、不启动服务 |
| `readiness` | 只读查询业务就绪状态；不启动管理器或业务服务 |
| `owner` | 通过独立监督通道查询状态、停止、恢复或关闭 CLI；管理 API 不可用时仍可使用 |
| `run-service` | supervisord 调用的内部服务执行入口 |
| `journal adopt` | 处理部署日志的状态冲突，属于运维命令 |

`run` 不是启动后立即退出的短命令，也不天然代表开发模式。
开发模式由 `APP_CLI_RUN_PROFILE=dev` 选择，优先使用 `[devrun]`；未设置时使用 `[run]`。
`run` 和 `serve` 都参与同一工作区的所有者协调，不应用另起进程的方式绕过已有所有者。
`serve` 同样以前台常驻方式运行，不会自动把自己变成后台服务；后台托管由调用方负责。
参数统一放在子命令后，例如 `app-cli run --workspace <workspace>`。

## CI/CD

`release-app-cli.yml` 发布，触发 tag：`app-cli-v*`（如 `app-cli-v0.2.0`）。
linux 双架构经 cargo-zigbuild（glibc 钉 2.17）；darwin 双架构 macos runner 原生编译。

## Version Management

单一事实源 = `crates/app-cli/Cargo.toml` 的 `version`。发版流程：改 Cargo.toml
版本 → 在该 commit 打同版本 tag `app-cli-v<VERSION>`（workflow 的 meta job 强校验
一致）。仓库内包 version 恒为占位 `0.0.0`，发布时 CI 注入。prerelease（版本含
`-`）发 `beta` dist-tag。

## How it works（与 rcoder-cli 的差异）

wrapper 同为 optionalDependencies 平台子包方案，但 `bin/cli.js` 用 **spawn + 信号
转发**（SIGTERM/SIGINT/SIGHUP）而非 execFileSync——`app-cli serve` 是长驻进程
（容器 PID1 语义），docker stop 时信号必须到达二进制才能优雅停（级联停 pingap
与各子服务）。
