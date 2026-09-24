# UserApp 子路径预览与静态资源

适用于 app-cli 0.3.8 及配套的 `workspace-manifest` 解析器。0.3.8 同步修复容器 supervisord 引擎：开发模式的 static + devrun 服务由开发进程承载，生产模式由内置静态服务承载。

## dev 与 prod 的路径配对

同一个 Vite SPA 在开发环境由 Vite 提供服务，在生产环境由 app-cli 提供 `dist` 静态文件。两种服务接收的路径不同：

| 形态 | Vite `base` | 代理策略 | 浏览器请求 → 上游路径 |
|---|---|---|---|
| dev，使用 `[devrun]` | `/react/` | `dev_strip_prefix = false` | `/react/src/main.tsx` → `/react/src/main.tsx` |
| prod，静态托管 | `/react/` | `strip_prefix = true` | `/react/assets/index.js` → `/assets/index.js` |

```toml
[proxy]
path = "/react"
strip_prefix = true
dev_strip_prefix = false
```

Vue 同理使用 `/vue/`。`dev_strip_prefix` 只在 dev 模式且该服务实际使用 `[devrun]` 时覆盖 `strip_prefix`；缺省仍沿用 `strip_prefix`。managed、extend 使用该规则，custom 模式由用户维护 Pingap 路由。

不要将子路径 Vite 的开发 `base` 设为 `/`：HTML 会引用域根的 `/@vite/client`、`/src/...`，请求可能落入工作区首页或其他服务。首页返回 200、编排任务 completed 都不能证明脚本正确加载。

app-cli 生成的 strip rewrite 同时处理 `/react` 与 `/react/` 形式的前缀，避免将 `/react/assets/x.js` 改写成 `//assets/x.js`。这里统一的是 rewrite；路由选择仍沿用已有的 Pingap 前缀匹配规则。

## 配置预览与重载

```bash
# 生成同一份同时包含两种策略的 release.lock，并预览 dev 生效配置
app-cli gen-lock --workspace . --dev

# 预览 prod 生效配置
app-cli gen-lock --workspace .
```

`--dev` 只选择此次预览，不把运行模式固定进锁文件。启动编排决定实际模式；`POST /v1/proxy/reload` 沿用当前编排的工作区和模式，不重新根据 owner 进程的启动环境猜测。

Pingap 配置目录由 `APP_CLI_PINGAP_RUNTIME_DIR` 指定；未指定时统一使用 `--log-dir` 下的 `pingap/`，启动、validate、reload、status 使用同一目录。

## 存量项目升级

1. 在使用新模板构建、启动应用前，升级包含新 manifest 解析器的 RCoder/file-server 和 app-cli 0.3.8（包括 agent-runner、app-runtime 对应镜像）。模板 npm 包可以先发布，但旧运行时的严格解析器会拒绝新字段，包发布不等于部署环境已兼容。
2. React/Vue 模板通过 `scripts/manifest-routing.mjs` 和 `smol-toml` 读取本模块 TOML 的 `[proxy].path`，在 dev/build 加载配置时生成 base。存量项目同步 helper、开发依赖、tsconfig 及 Vite 接线，保留业务插件；不再写死 base 或通过环境变量存储第二份路径。
3. 在该项目 manifest 中增加 `dev_strip_prefix = false`，保留 prod 的 `strip_prefix = true`。
4. 开发健康插件同时响应内部探测 `/health` 和带 base 的 `/react/health`；Vite 接到 `/react` 的 GET/HEAD 时跳转到 `/react/`，保留查询参数。生成新的锁文件后重启开发服务。不要直接编辑运行目录下的生成态 Pingap 配置。
5. 检查前缀内的 HTML、JS Content-Type、HMR WebSocket 握手和健康 JSON；重新构建后检查 prod 的 `/react/assets/...`。

模板更新不会改写已经创建的工作区；存量项目需要同步第 2–4 步。已有项目不使用新字段时仍能解析，但旧 Vite base 的问题不会因此自动消失。

## 无容器的真实进程回归

`tools/verify_userapp_subpath.py` 使用真实 app-cli、Pingap、Vite 进程，在同一 workspace 创建 React 和两个 Vue 模块；其中一个 Vue 创建后只修改 TOML 路径，Vite 配置保持不变。验证 dev 启动 → 各模块内容/JS/健康/HMR → 代理重载 → 停止 → prod 静态托管 → 清理监听端口。各模块带独立标记，避免串路由仍误报通过。

同类前端由 `template-cli add frontend-vue3-vite --service-id portal-web --path /portal/` 创建。
日常改公共路径只改该服务 TOML，改后重启 dev、重新构建 prod，同时更新业务导航。
已有 Vite 进程及已构建 dist 不会仅因 TOML 改变就自动刷新。

前置条件：Python 3.11+、Node、pnpm；已构建和打包的 template-cli；React/Vue 模板目录已安装依赖；与 app-cli 配套的 Pingap 0.14.3。9080、9081、3018 必须空闲。脚本在临时目录生成项目、复制每个模块独立的依赖目录并保留日志，不修改模板源码；pnpm 运行时可能校正临时项目的安装状态。

```bash
cargo build --manifest-path crates/app-cli/Cargo.toml --bin app-cli
python3 tools/verify_userapp_subpath.py \
  --app-cli crates/app-cli/target/debug/app-cli \
  --pingap /path/to/pingap \
  --templates /path/to/userapp-workspace-template
```

这是宿主机 builtin 编排与路由回归，不替代浏览器实际渲染、容器 supervisord、Java → Gateway → 预览域名的部署验收。脚本检查 HMR 握手，不宣称验证过页面编辑后的热更新效果。
