# @nuwax-ai/file-server

Rust 实现的 `file-server`（文件 / git / 上传 / skills / computer 全功能 HTTP 服务，[rcoder](https://github.com/nuwax-ai/rcoder) 的一个 crate）的分发包，面向 **Electron 客户端**等 Node.js 场景。

本包只含一个 JS launcher，**原生二进制不在 npm 包内**——安装时（或首次运行时）按宿主平台从阿里云 OSS 下载预编译二进制并缓存到 `node_modules/.cache/`，electron-builder 打包时随 App 分发，终端用户无需联网。

## 安装

```bash
npm install @nuwax-ai/file-server
```

`npm install` 会自动触发 postinstall 预下载当前平台的二进制。下载失败不会中断安装，会在首次运行时重试。

## 作为 CLI 使用

```bash
# 直接运行（透传所有参数给原生二进制）
npx file-server --port 60000
# 或装到全局
npm install -g @nuwax-ai/file-server
file-server --port 60000
```

## 在 Electron 中以 sidecar 方式使用（推荐）

通过编程式 API 拿到二进制路径，由 Electron 主进程 `spawn`：

```js
const { spawn } = require("node:child_process");
const { ensureBinary } = require("@nuwax-ai/file-server");

async function startFileServer() {
  const binary = await ensureBinary(); // 缺失则下载，已缓存则直接返回
  const child = spawn(binary, ["--port", "60000"], { stdio: "inherit" });
  return child;
}
```

同步获取（不触网，缺失返回 `null`）：

```js
const { resolveBinaryPath } = require("@nuwax-ai/file-server");
const binary = resolveBinaryPath();
```

### ⚠️ electron-builder 必须解包二进制

Electron 默认把 `node_modules` 打进 `app.asar`，**asar 内的原生二进制无法直接执行**。需在 `package.json` 的 `build` 配置里把缓存目录解包：

```json
{
  "build": {
    "asarUnpack": [
      "node_modules/@nuwax-ai/.cache/file-server/**"
    ]
  }
}
```

解包后路径会带 `app.asar.unpacked` 前缀，`ensureBinary()` 返回的就是正确路径，无需手动拼接。

## 支持的平台

| OS | 架构 | Rust target | 归档 |
|---|---|---|---|
| macOS | arm64 / x64 | `aarch64-apple-darwin` / `x86_64-apple-darwin` | tar.gz |
| Linux | x64 (glibc) | `x86_64-unknown-linux-gnu` | tar.gz |
| Linux | x64 (musl/Alpine) | `x86_64-unknown-linux-musl` | tar.gz |
| Linux | arm64 | `aarch64-unknown-linux-gnu` | tar.gz |
| Windows | x64 | `x86_64-pc-windows-msvc` | zip |

Linux x64 的 gnu/musl 由 [`detect-libc`](https://www.npmjs.com/package/detect-libc) 自动判定。

## 环境变量

| 变量 | 说明 |
|---|---|
| `FILE_SERVER_BINARY` | 指向自定义/预置二进制路径，跳过下载（调试或离线分发） |
| `FILE_SERVER_TARGET` | 强制覆盖 Rust target triple（如交叉打包时 `FILE_SERVER_TARGET=x86_64-pc-windows-msvc` 从 mac 下 windows 版） |
| `FILE_SERVER_SKIP_DOWNLOAD=1` | 跳过 postinstall 预下载 |

## 在线/离线与代理

下载走 `https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server/v{version}/file-server-{version}-{target}.{ext}`，遵守系统 `HTTP_PROXY`/`HTTPS_PROXY`。若装包环境无外网，可用 `FILE_SERVER_SKIP_DOWNLOAD=1` 安装，再把对应二进制手动放到 `node_modules/@nuwax-ai/.cache/file-server/{version}/file-server[.exe]`。

## 排错

- **下载 404**：确认该版本已在 OSS 上线（由 rcoder 仓库的 `release-file-server.yml` CI 上传），且版本号与本包 `package.json` 一致。
- **`tar`/`Expand-Archive` 找不到**：tar.gz 解压依赖系统 `tar`（macOS/Linux 自带，Windows 10+ 自带）；zip 依赖 PowerShell。极少缺。
- **权限不足**：确保对 `node_modules` 有写权限；或改用 `~/.cache` 自行管理并设 `FILE_SERVER_BINARY`。
