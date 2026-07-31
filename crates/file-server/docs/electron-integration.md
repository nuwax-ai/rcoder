# file-server —— Electron / prepare 脚本集成契约

面向把 file-server 作为 **sidecar 二进制**集成进 Electron 客户端（如 nuwaclaw）的消费者。
配套的 npm 包 `@nuwax-ai/file-server` 走 postinstall 下载，适合通用 Node 场景；但 Electron
客户端通常对原生二进制走 **prepare 脚本（构建期下载）→ extraResources 打包 → 运行时按平台解析**
的范式（不依赖 npm postinstall、绝不运行时下载）。本文定义后者所需的产物契约。

## 产物与事实源

每次发版（CI：`release-file-server.yml`，tag `file-server-v*`）会在阿里云 OSS + GitHub Release 同时产出：

- 二进制归档：`file-server/v{ver}/file-server-{ver}-{rustTarget}.{tar.gz|zip}`
- **版本清单 manifest**（单一事实源，带 sha256）：
  - 按版本：`https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server/manifest/{ver}.json`
  - 最新稳定：`.../file-server/manifest/latest.json`
  - 最新预发布：`.../file-server/manifest/beta.json`

> 优先用 manifest 选资产 + 校验完整性，而不是写死版本号 + 拼文件名。版本事实源始终在 file-server 仓库侧。

## manifest 结构

```json
{
  "name": "file-server",
  "version": "0.1.1",
  "commit": "<git sha>",
  "publishedAt": "<RFC3339>",
  "prerelease": false,
  "targets": {
    "darwin-arm64": {
      "rustTarget": "aarch64-apple-darwin",
      "archive": "file-server-0.1.1-aarch64-apple-darwin.tar.gz",
      "sha256": "<64 hex>",
      "size": 34567890,
      "url": "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server/v0.1.1/file-server-0.1.1-aarch64-apple-darwin.tar.gz"
    }
  }
}
```

`targets` 的 key 是**消费者 plat-arch 风格**（与 nuwaclaw 的 `PLATFORM_MAP` 对齐），不是 Rust triple：

| plat-arch key | rustTarget | 归档 |
|---|---|---|
| `darwin-arm64` | `aarch64-apple-darwin` | tar.gz |
| `darwin-x64` | `x86_64-apple-darwin` | tar.gz |
| `linux-x64` | `x86_64-unknown-linux-gnu` | tar.gz |
| `linux-x64-musl` | `x86_64-unknown-linux-musl` | tar.gz |
| `linux-arm64` | `aarch64-unknown-linux-gnu` | tar.gz |
| `win32-x64` | `x86_64-pc-windows-msvc` | zip |

> nuwaclaw 的 `RESOURCE_PLATFORM_KEY_MAP` 会把 `win32-x64` 重命名为 `windows-x64`；落地目录用它自己的命名，但选资产时按 `process.platform-process.arch` 拼 key 查 manifest 即可。

## prepare 脚本推荐流程（伪代码）

```js
// 1) 选 manifest：pin 版本用 {ver}.json；跟最新用 latest.json（生产建议 pin）
const MANIFEST_URL =
  "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server/manifest/0.1.1.json";
const m = await (await fetch(MANIFEST_URL)).json();

// 2) 按 host 选 target（plat-arch key）
const key = `${process.platform}-${process.arch}`; // 如 darwin-arm64 / win32-x64
const t = m.targets[key];
if (!t) throw new Error(`no target for ${key}`);

// 3) 下载 + 校验 sha256
const buf = await (await fetch(t.url)).arrayBuffer();
const sha = crypto.createHash("sha256").update(Buffer.from(buf)).digest("hex");
if (sha !== t.sha256) throw new Error(`sha256 mismatch for ${t.archive}`);

// 4) 解压到 resources/file-server/{plat}-{arch}/bin/file-server[.exe]
const resKey = key === "win32-x64" ? "windows-x64" : key; // 对齐 nuwaclaw 资源目录命名
const dest = path.join("resources/file-server", resKey, "bin");
await extractTarGzOrZip(buf, dest); // tar.gz 用 tar；zip 用 Expand-Archive

// 5) 写 .version + 用二进制自身核验版本（防“标记更新但二进制没换”）
fs.writeFileSync("resources/file-server/.version", m.version);
const v = execSync(`${dest}/file-server --version`).toString().trim(); // → "file-server 0.1.1"
if (!v.endsWith(m.version)) throw new Error(`binary version mismatch: ${v}`);
```

## 运行时约定（给 spawn/服务管理）

- **端口**：二进制读 `FILE_SERVER_PORT` 或 `PORT` 环境变量。nuwaclaw 默认 `60005`，spawn 时传 `PORT=60005` 即可，**无需改 file-server 默认端口**。
- **健康检查**：`GET /health` → `200 {"status":"ok",...}`，启动后据此判活。
- **优雅关闭**：Unix 收 `SIGTERM`/`SIGINT` 优雅退出；Windows 收 Ctrl-C。ManagedProcess 的 SIGTERM→SIGKILL 升级可直接套用。
- **`--version`**：打印 `file-server {version}` 后退出 0，不启动服务。供依赖检查 / prepare 脚本核验。
- 其余配置（工作空间目录等）走环境变量，见 [`src/config.rs`](../src/config.rs)。Electron 场景一般把这些指向 app userData 下的可写目录。

## nuwaclaw 迁移（参考，不在本仓库范围）

替换现有 Node 版 `nuwax-file-server` 需要动 nuwaclaw 侧：新写 `prepare-file-server.js`（按上文流程）、
`binaryLocator` 的 `getNuwaxFileServerBundledDir` 改为按 plat-arch 查二进制、`serviceManager` 的 spawn
从「`ELECTRON_RUN_AS_NODE=1` 跑 `server.js`」切到「直接 spawn 二进制」、`extraResources` / 依赖版本读取相应调整。
这些改动都在 nuwaclaw 仓库，本文档仅给出 file-server 侧的产物契约。
