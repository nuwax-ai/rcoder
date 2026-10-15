# RCoder CLI 的 npm 分发

本目录维护 `rcoder-cli` 与 `@nuwax-ai/app-cli` 的 npm 主包、启动 wrapper 和平台二进制包模板。安装使用说明与包维护分别见下文；app-cli 的完整命令行为集中在[使用指南](../docs/development/app-cli.md)。

## 安装

```bash
npm install -g rcoder-cli
rcoder-cli --help

npm install -g @nuwax-ai/app-cli
app-cli --help
```

预发布版本使用 `@beta`。支持的平台以对应主包、wrapper 与发布 workflow 为准；安装时保留 optionalDependencies，以便 npm 安装当前平台二进制。

## 包结构与启动

| 包 | 主包目录 | 平台目录 | 作用 |
|---|---|---|---|
| `rcoder-cli` | [rcoder-cli/](rcoder-cli/) | `rcoder-cli-*` | 本机测试 ACP Agent 的 CLI |
| `@nuwax-ai/app-cli` | [app-cli/](app-cli/) | `app-cli-*` | UserApp 构建与服务管理器 |

主包的 `bin/cli.js` 读取操作系统和架构，定位 optional dependency 中的二进制，再转发命令参数。app-cli wrapper 使用 spawn 并转发 SIGTERM/SIGINT/SIGHUP，支持 `serve` 常驻运行；平台包随附配套 Pingap，调用方显式设置 `APP_CLI_PINGAP_BIN` 或 `--pingap-bin` 时使用指定配置。

`app-cli run` 向独立常驻 owner 提交启动请求并等待本次结果；已有 owner 会被复用，没有可用 owner 时会自动启动管理进程。CLI 请求返回后管理仍运行。它与前台常驻的 `app-cli serve` 职责不同，详见[源码启动与重复运行](../docs/development/app-cli.md#当前源码启动与重复运行)。

## 发布入口

| 产物 | Workflow | Tag |
|---|---|---|
| rcoder-cli 正式版本 | [release.yml](../.github/workflows/release.yml) | `v<VERSION>` 或 `<VERSION>` |
| rcoder-cli beta | [release-beta.yml](../.github/workflows/release-beta.yml) | `v<VERSION>-beta*` 或 `<VERSION>-beta*` |
| app-cli | [release-app-cli.yml](../.github/workflows/release-app-cli.yml) | `app-cli-v<VERSION>` |

app-cli 版本以 [crates/app-cli/Cargo.toml](../crates/app-cli/Cargo.toml) 为单一来源。发布 tag 必须对应同版本源码提交，workflow 会核验一致性；稳定版本发 `latest`，预发布版本发 `beta`。主包与平台子包的仓库版本使用占位 `0.0.0`，发布时注入实际版本。

app-cli 的 Linux 双架构构建使用 cargo-zigbuild，并设 glibc 2.17 基线；macOS 与 Windows 使用各自构建 runner。Pingap 与 app-cli 配对检查由发布 workflow 执行。源码或版本号改变不等于已发布，应分别核验主包、平台子包、dist-tag 与实际安装版本。
