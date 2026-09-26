# File-server A/B 对照

在本机用独立 Docker Compose project 并行运行 Rust `file-server-proxy --embed --policy all_rust` 和指定提交的 TypeScript `nuwax-file-server`，对两边发送同一组 HTTP 请求并比较响应、文件状态。它不经过 RCoder 代理或 TS 上游，避免路由转发掩盖实现差异。

## 运行

```bash
make file-server-ab
```

默认执行离线 `core` 套件。需要比较 Git 或模板依赖安装/构建/开发服务时显式选套件：

```bash
make file-server-ab AB_SUITE=git
make file-server-ab AB_SUITE=build
make file-server-ab AB_SUITE=all
```

`build` / `all` 会从两份 ZIP 模板创建项目，访问 npm registry 安装依赖并构建，再验证 dev server 的启动、HTTP 可达、日志分页、日志缓存接口、端口池登记、keep-alive、重启和停止；也会对照构建错误解析。项目创建包含模板解压和初始 Git 提交，驱动为这一步单独留出 120 秒，避免慢磁盘上的 30 秒通用请求预算过早中断后继续请求尚未初始化完成的项目。模板依赖安装仍在每个被测项目内真实执行，不把项目 `node_modules` 预装进镜像。

可选参数：

```bash
make file-server-ab \
  AB_SUITE=core \
  AB_TS_SOURCE=/path/to/nuwax-file-server \
  AB_TS_REF=main \
  AB_PNPM_VERSION=10.34.5 \
  AB_PNPM_CACHE_VOLUME_PREFIX=rcoder-file-server-ab-pnpm \
  AB_PNPM_NETWORK_CONCURRENCY=32 \
  AB_BUILDER=mac-arm64 \
  AB_RUST_PORT=61101 AB_TS_PORT=61100 \
  AB_KEEP=1
```

国内网络可将 `DOCKER_MIRROR` 放入被 Git 忽略的 `.env.local`，或仅在调用时传给 Make。设置镜像前缀后，Rust 构建及运行阶段使用 `${DOCKER_MIRROR}rust:trixie`，并从对应的 `node:22-trixie-slim` 复制 Node；TS 服务运行层也使用 Node Trixie，以保证 Git、Node 和 glibc 环境一致。apt 安装前使用 LinuxMirrors 将 Debian 源配置为阿里云（不升级基础系统）；下载失败通过 `curl -f` 和 `pipefail` 明确报错。镜像构建中的 npm 与 pnpm 均显式使用 npmmirror，并配置有限重试；TS 服务镜像的冻结锁文件安装使用 32 路网络并发，包内容由 BuildKit store 缓存。

pnpm 包内容 store 与 registry 元数据缓存使用持久 Docker named volume，不走 OrbStack 的 macOS host-bind 文件共享层。Rust 与 TypeScript 分别使用独立缓存，按 pnpm 版本、目标平台和宿主 UID/GID 隔离；这样每一侧第一次安装都由自己的 API 从 npmmirror 获取依赖，不会因另一侧先运行而白用对方刚下载的包。卷名以 `AB_PNPM_CACHE_VOLUME_PREFIX` 为前缀；Compose 每轮结束只删除项目卷，不删这些外部缓存卷。两侧都把自己的包 store 挂载到 `/pnpm-cache`，把元数据缓存挂载到 `/pnpm-metadata`；启动器检查实际 store 路径、registry 和网络并发配置。

工具链基础镜像只装 Rust、Node 与 pnpm，不包含模板项目的 `node_modules`。模板依赖安装由被测 Rust/TypeScript API 触发；两边使用 npmmirror、相同 PNPM 版本和网络并发。fixture 不指定导入策略；两侧当前生产 API 都会生成 `package-import-method=copy`，对照保留该实际行为。source-lock 模式下两边各自从原始无锁 fixture 解析并安装，安装结果和下载耗时保留在 API 日志中。

正常重复运行使用固定的 `AB_PNPM_CACHE_VOLUME_PREFIX`，会复用每侧的 PNPM 内容与元数据缓存；只有显式换新前缀时才会做冷缓存比较，不要为普通回归每次换前缀。冷缓存安装耗时包含 registry 下载与解析。TS Compose 配置未启用 `FAST_RESTART_ENABLED` 时，`restart-dev` 会走 Full restart，删除项目 `node_modules` 和 lockfile 后再安装；这是被测 TypeScript 服务的重启行为，不是工具链基础镜像在安装模板依赖。

每侧的 `project-workspace` 与 pnpm 缓存都使用独立 Docker 管理卷；日志、报告和上传文件保存在宿主机 `target/`。这样 Vite 项目解压、`node_modules` 和 pnpm store 的大量小文件读写不会经过 OrbStack 的宿主机目录共享层；模板依赖仍在被测项目中真实安装，绝不预装进共享基础镜像。

默认宿主机端口由 Docker 动态分配并只绑定到 `127.0.0.1`。指定 `AB_RUST_PORT` / `AB_TS_PORT` 后使用固定端口。每轮都有唯一 Compose project、宿主机数据目录和报告目录；镜像标签按内容复用。运行结束会删除本轮容器及其项目工作区卷；对照差异或服务请求失败时仍保留宿主机日志和报告。需要检查容器内项目文件时，用 `AB_KEEP=1` 保留容器与项目工作区卷。

使用 `make file-server-ab-build` 只构建镜像，`make file-server-ab` 构建缺失镜像后执行对照。Bake 将两侧共同的工具链作为 `target:toolchain` 依赖，只导出 Rust 和 TS 两个最终镜像，基础阶段只留在 BuildKit 缓存，不单独打包和导入。Rust 源码通过构建期 bind mount 提供，专用 dockerignore 排除非构建文件，target 缓存使用 `sharing=locked` 防止并发写入。TS 先复制依赖清单安装，再复制源码，源码变化不触发重新安装依赖。

共享工具链基础镜像以 `${DOCKER_MIRROR}rust:trixie` 为 Rust 基础，从镜像仓库的 Node Trixie 复制 Node 22，并经 npmmirror 安装 pnpm；工具链配方按 Dockerfile、版本、平台和 UID/GID 生成稳定指纹，重复运行复用层缓存。基础镜像不带模板项目 `node_modules`。TS 服务镜像按 `pnpm-lock.yaml` 安装自身运行所需的生产依赖，BuildKit pnpm store 用于缓存服务自身的包；React/Vue 模板的 `node_modules` 仍在 A/B 请求中由各 API 真实安装，不会预装进镜像。依赖安装耗时由服务日志记录；首次冷安装与后续命中每侧缓存的安装都属于 A/B 被测行为，不另行预热。构建图单独放在 `docker-bake.hcl`；运行用的 `compose.yaml` 只有 image，没有 build。缓存初始化和驱动使用 `run --pull never`，不会再次构建。最终镜像按源码、工具链和构建配方生成内容标签，重复执行直接复用，并按不可变 image ID 启动。清理仅删除本轮容器与临时工作卷，保留镜像缓存及独立 pnpm 缓存；需要回收镜像时由用户按标签手动清理。

启动器在服务就绪后核对两侧 Node、pnpm、Git 版本，以及 pnpm 实际解析出的 registry、独立持久 store 路径和网络并发配置。TS 服务依赖层只由 `package.json` 与 `pnpm-lock.yaml` 决定，源文件变化不会触发重装；BuildKit store 在该依赖层失效时复用已下载内容。模板依赖安装保留为被测路径，运行时持久 store 位于 Docker named volume，不进入 Git 或工具链基础镜像。

`core` 使用 Compose 中的 `x-file-server-ab-shared-profile`：两侧的大小上限、遍历/归档排除、图片扩展名和 Git 忽略规则由同一个 YAML 锚点注入，基于 TS `env.test` 的共同设置。启动器在构建前要求两侧都含有全部共享配置键，并比较同名环境变量的值；缺键或值不同都会 fail fast。TS-only 的 `TOP_LEVEL_NOISE_PATTERNS` 单独按 TS 测试配置注入；它对应的 Rust 行为差异需由场景验证，不能靠空 `env.ab` 隐去。`REQUEST_BODY_LIMIT` 统一设为 `1024mb`，因为 Rust 实现的硬上限为 1 GiB；core 请求远小于此限制。`manifest.json` 的 `configuration_profile` 标识本配置版本。

## 报告

报告位于 `tests-e2e/reports/file-server-ab/<run-id>/`，包括：

- `manifest.json`：Rust/TS 源码身份、镜像 ID、Node/pnpm/Git 版本、运行架构、模板 ZIP 哈希、两侧独立 PNPM store/metadata 卷名、配置 profile 和规则文件哈希。
- `requests.jsonl`：每个请求的 start 行（发送前写入，含 `request_id`）与 result 行（同一 `request_id`，含状态、选定响应头、耗时、完整响应哈希、原文引用或传输错误）；中断时能看到"已开始未返回"的请求。
- `cases.jsonl`：每个场景完成即增量追加的 CaseResult，汇总阶段失败也不会丢失已完成场景。
- `bodies/`：请求与响应原文；单个文件最多保存 2 MiB，截断状态、完整字节数和完整 SHA-256 仍记录在 JSONL。
- `state/`：请求执行前与结束后的两侧工作区树、文件内容摘要、权限与软链接；`git`/`all` 另保存每个 fixture repo 的 HEAD、refs 对应 tree、index entries 和 porcelain 状态。快照对三类既有动态内容做形状级归一并在文件摘要中以标记显式呈现：`.dynamic_add.lock`（两侧均写入 `<epoch-millis>` 时间戳标记，按"纯数字+可选换行"形状比较，非该形状仍按字节比较）、生成的 `.npmrc`（两侧均在 `# 自动生成于` 注释行嵌入秒级本地时间，仅归一该行，其余内容按字节比较；该归一同样作用于 ZIP 语义条目中的 `.npmrc`）与 `.zip` 落盘产物（按与 ZIP HTTP 响应相同的条目语义摘要比较，解析失败的 zip 仍按原始字节比较）。存在性、类型和权限始终参与比较。
- `diff.json`、`summary.md`：机器可读差异及人工可读总览；错误响应只归一化精确路径 `/error/requestId` 和 `/error/timestamp`，两侧原值仍保存在正文证据中。
- `route-coverage.json`：TS/Rust 路由交集、已覆盖/待覆盖状态。`executed_cases` 只包含"场景完成成对请求**且**真实 method+路由模板匹配到该场景 API 请求记录"的场景；仅 dev-server 探针（如 `/`）不计入 API 路由证据。`execution_status` 区分 `not_run`、`blocked`（依赖场景已失败或被阻断）、`failed`（有断言失败或传输错误）、`partial`（本轮计划内场景未全部执行）、`unverified`（场景名已登记但 Rust/TypeScript 任一侧缺少匹配的 API 请求记录，明细按侧命名，属对照器/清单映射缺陷，会使整轮失败）、`completed`（双侧执行完成且有请求证据，不代表两端语义一致）。`coverage_inconsistencies` 列出所有 `unverified` 明细；`unexecuted_planned_routes` 列出本轮计划内未双侧执行的路由——存在时即使全部已执行场景两端一致，运行也判 incomplete 失败。路由覆盖在**每个场景完成后增量落盘**；场景执行、最终文件/Git 快照与汇总整理处于同一错误收束内，任一阶段失败都会写入带 `incomplete_reason` 的部分 `diff.json`/`summary.md` 与 `attempts.json`（区分已开始与已返回的请求计数），不会因结尾退出丢失已完成场景。
- `logs/compose.log`：Compose 服务日志，包含被测 API 实际触发的 PNPM 安装输出；没有单独的模板依赖预热安装。

镜像解析、构建或容器启动失败时不会生成比较通过结果；启动器会写 `runner-failure.json`，包含失败阶段和类别，并在 `summary.md` 中明确说明没有产生对照结果。失败类别保守分列：只有镜像解析、缓存卷准备、容器启动和运行时校验阶段计为 `environment`；镜像构建失败单列为 `image-build`（可能来自任一侧源码缺陷或网络，需读 `logs/build.log` 归因）；对照阶段失败为 `comparison-runner`，不会自动定性为环境故障。健康前置失败时驱动写 `environment-errors.json`，Compose 服务日志仍由启动器保留。HTTP 传输错误保存在 `diff.json`，会使命令失败。

## 场景依赖与失败隔离

场景按真实数据依赖声明先后关系（如 files-update 之后的读取、项目创建之后的构建、Git seed 链、build 套件每模板的生命周期链）。上游场景出现**断言失败或传输错误**时，依赖它的下游场景标记 `blocked_by` 并跳过执行，不再对已损坏状态发送级联请求；仅外观性差异（如 ETag 值不同）不会阻断下游。被阻断场景在 `diff.json` 的 `blocked` 差异与 `summary.md` 的 `blocked` 计数中单独呈现，不计入未分类差异，也不能用差异规则豁免；其根因失败仍使门禁失败。其他独立项目、仓库或模板链继续执行。build 套件中一侧 start/restart 失败时，已成功启动一侧的 dev server 会通过单独的 `*-stop-dev-cleanup` 请求尽力停止，清理请求保留在 `requests.jsonl` 中但不冒充对照场景。

报告继承启动环境的 `umask 077` 权限，不应手工加入凭据。当前 `core` 场景不读取任何真实凭据。

## 差异规则

默认所有差异都视为未分类并使命令失败。经人工确认的预期差异可写入 `diff-rules.json`，规则必须精确匹配 `case`、JSON Pointer `path`、`kind`、Rust 值和 TypeScript 值，并附原因、审核者和未过期日期；不接受通配路径，也不允许把传输错误归为预期差异。值缺失用 `{"$missing":true}` 表示。命中规则只改变分类，不会删除原始响应或 diff。

## 套件与覆盖边界

- `core`：健康/API 版本、React/Vue 模板初始化与读取、项目全量文件替换、复制/删除/导出/上传、Computer workspace 创建/删除、skills ZIP、项目 ZIP 导入和模板初始化、合成 package 生成/清理、日志读取、工作区 ZIP 下载/创建，以及 `files-update` 的 create/modify/rename/delete 和 URL 解码。multipart 单/批量二进制上传会静态读回并逐字节核验；所有 ZIP 响应按条目路径、类型、权限和内容摘要比较，不比较压缩顺序/时间戳。另覆盖文件列表/resolve/search/metadata 边界（带首尾空格文件名的 metadata 差异按已批准分歧做场景级比较：断言 Rust 精确寻址返回完整元数据、TS trim 后 ENOENT）、静态普通/Range 读取和基础文件系统操作。除 `install-project` 的无依赖 pnpm 场景外，不访问 npm 外网。
- `git`：通过 HTTP 对照 init、status、add、commit、file-content、branch create/delete、tag、log、worktree/staged diff、unstage、checkout、discard、revert，以及 mixed/hard/soft reset。另用系统 Git 为两侧独立 fixture 准备相同的真实 merge-conflict index，再通过 HTTP 对照 `status.conflicted`；当前 API 没有 merge 操作端点，因此不把 fixture 准备命令当成被测 API。每个会改变历史或工作树的流程使用独立 pageApp fixture，避免一个实现的失败污染其他场景；最终比较 refs 对应 tree、HEAD tree、index entries、工作区状态和文件树。Rust 服务使用 gix，TS 服务使用镜像内系统 Git；驱动只用系统 Git读取最终仓库状态及准备对称 fixture，不参与被测 API 操作。diff 正文仅做两类已验证的 hunk 格式归一：可省略的单行数量 `,1`，以及 `new file mode` 文件块内 gix `-1,0` 与 git `-0,0` 的顶部空范围起点约定；零行数量 `,0` 始终保留（不与省略的单行数量混同），块外空范围起点不归一，所有其他行、范围和摘要仍逐字比较；原始 HTTP 正文继续保存。
- `build`：分别用两份模板走项目初始化、依赖安装、production build、产物静态读取、start-dev、真实页面 HTTP、开发日志分页、日志缓存查询/清理、端口池状态、keep-alive、restart-dev 和 stop-dev，并对照构建错误解析。依赖 registry 网络；报告记下环境版本与错误。`get-dev-log` 逐行校验每侧页面契约（行号从请求的 startIndex 连续、content 为字符串、totalLines 覆盖末行、`dev-temp-<epoch-millis>.log` 文件名形状），并用第二页查询（`build-react-get-dev-log-page-2`，startIndex=首页末行+1）验证分页不重不漏——日志可能增长，不要求两次 totalLines 相等。日志正文与 `logFileName` 是两侧各自的安装/vite instrumentation（结构化 pnpm 事件 vs 原始输出、体量不同），按场景级语义比较归一：页面契约（非空分页、行号、totalLines 一致性）由独立断言保证，`success`/`startIndex` 仍逐字比较，动态的日志正文、`logFileName`、行量派生的 `totalLines` 及正文派生的 `content-length`/`etag` 响应头随之归一。TS Compose 未启用 `FAST_RESTART_ENABLED` 时 restart 走全量重装（删除 node_modules 与 lockfile 后重装），Rust restart 为 stop+start 保留 lockfile；两侧 lockfile 终态差异是该已记录行为差异的后果，不以忽略规则抹除。
- `all`：顺序执行以上套件。路由清单按当前 TypeScript 基线快照维护；没有 A/B 场景的共同路由明确标为 pending。

路由清单的 `typescript_revision` 必须与本次准备的 TypeScript Git 提交一致；基线变化时 Make 运行会在发请求前失败，要求先复核并更新路由清单。当前清单中的 76 条共同路由均登记了至少一个场景；截至 2026-09-25，`core` 的真实 A/B 报告已执行此前待测的 27 条路由。`covered` 只表示场景实际执行过，不表示两端语义一致；是否一致以差异审阅和精确规则为准。

这不是“所有路由都已覆盖”的声明。真实结果以报告中的 `route-coverage.json` 与 `requests.jsonl` 为准；Rust-only `/api/v1/userapp` 由既有 UserApp 测试单独覆盖。

### 本地构建优化边界

`file-server-ab-build` 不创建测试卷或处理模板 ZIP。共享工具链先装软件再配置运行用户，UID/GID 改变不触发 pnpm 重装。npm、TS pnpm 和 Rust target 使用持久构建缓存；写缓存采用互斥挂载。本地对照镜像不生成 provenance attestation，仍记录源码、实际 image ID 和 BuildKit metadata；这不是生产发布入口。构建日志保存在每轮报告的 `logs/build.log`，纯复用镜像时不产生构建日志。

### 依赖缓存与证据口径

- TS 服务自身的 `node_modules` 留在镜像依赖层，不挂宿主机目录覆盖；模板项目的 `node_modules` 位于每轮独立项目卷，pnpm 内容/元数据缓存位于每侧独立持久卷。Mac 不直接挂本机 `node_modules`，避免跨系统二进制和共享目录小文件开销。
- `planned_in_this_run` 是计划；`executed_cases` 来自实际完成的成对请求结果，并与真实 method/路由模板请求记录交叉核验。`completed` 仅表示执行完成且证据一致，不表示两端一致；被阻断场景单列为 `blocked_cases`，中途退出的增量报告保留已执行部分，不虚报执行。
- `transport_errors` 表示归因未定的传输失败，不自动计为环境故障；健康前置失败另记录前置错误。传输失败仍使门禁失败。
- `headers_elapsed_ms` 是响应头耗时，`elapsed_ms` 覆盖正文读取。记录用于诊断，不作为两实现性能胜负依据。
- 动态 JSON 字段允许值不同，但不能隐去字段缺失或类型改变；场景级关联不变量仍需专门断言。


## 响应头协议语义与差异分类

响应头不是逐字比较，也不是全局忽略：`compare_exchange` 对四类头做协议级校验（`header_protocol_verdict`，均有单测锁定）：

- **Content-Type**：媒体类型必须相同；charset 显式 `; charset=utf-8` 与隐式（JSON 默认 UTF-8）等价；charset 实质不同或格式非法按侧报断言失败。
- **ETag**：实体标签不透明，两侧格式合法即等价；静态内容要求两侧一致存在（校验器属于该契约）；非静态路由允许"TS Express 框架默认弱 ETag vs Rust 无"的存在性差异，条件请求契约由 `read-project-static-file-if-none-match` 等专属场景强制。
- **Content-Length**：每侧必须等于自身实际传输字节（304 等除外），两侧各自正确即按派生等价；与自身正文不符按侧报断言失败。
- **Last-Modified**：静态内容两侧各为独立创建的 fixture 文件 mtime，格式合法即等价；非静态逐字比较。
- 其余头（Cache-Control、Accept-Ranges、Location、CORS 头等）始终逐字比较。

`summary.md` 的 **Classified outcome** 段把每个差异归入：Rust 契约失败（`/assertions/rust/*`）、TS 已知缺陷（`/assertions/typescript/*`，如 git-revert 500、stop-dev 500）、已批准差异（命中 `diff-rules.json`）、未解释差异、传输失败与 blocked。分类只改善定位，strict 退出规则不变。

当前 `diff-rules.json` 登记 25 条已批准差异（值逐条精确匹配并带复核者与到期日）：越根 symlink 边界（Rust 保留边界、TS 越根为已知缺陷）×5、UTF-8 字节数 fileSize ×1、TS `.tmp`/上传 zip 残留 ×7、Rust 复制保留嵌套 lockfile ×1、`.agents/.sync_version` 标记 ×2、TS 删除清单含日志目录 ×1（Java 消费者只读 success/message，2026-09-26 核对）、log-cache-stats Rust 附加字段 ×8（agent-platform 无消费者）。**仍然未分类**的已知项：TS git-revert 500 与 stop-dev 500（失败断言如实保留，不豁免冒充通过）、project-copy Git 历史（待产品决策）、dev 日志缓存口径（Rust 将 dev 日志写入日志缓存：stats 的 `cacheSize`/容量字段与 `get-dev-log` 的 `cacheHit`/消息文案不同；Java `CustomPageBuildController` 会透传 `cacheHit` 到前端，属消费者可见差异，待产品裁定）、export zip 的 TS 框架头（POST 下载上的 Accept-Ranges/Last-Modified/Cache-Control）。`get-dev-log-page-2` 的 `startIndex` 归一是因为各侧回显自己请求的行号（两侧行数不同），回显正确性由 `validate_log_page` 断言。

版本端点：各端报告自身实现的契约线版本（Rust 报 `API_CONTRACT_VERSION`，非 crate 构建号——Java `FileServerVersionSupport` 以 `>=1.4.0` 门禁 agent-store 能力）；A/B 断言语义版本形状而非跨端相等。
