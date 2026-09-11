# userApp 定向修复报告

**定向修复与本地完整验收完成：严格 userApp 31/31、Compose 54/54 均通过。真实 K8s 未执行。**

基线：`feature-userapp / d886b67`。生产源码 SHA256：`53e1726039230e4b808ae553c8fb3459a4cdce3cd4c27f88485d9d468d67b03e`。以下是已实施变更；代码片段指修复后的关键位置。

## R01 · P0 · 失败创建可能误删竞争赢家

位置：[crates/app_manager/src/lifecycle/create.rs:214](/Users/soddy/Documents/git-workspace/rcoder/crates/app_manager/src/lifecycle/create.rs:214)

触发与后果：两个副本同时创建同一 app 时，失败请求过去会按 app_id 删除另一请求刚建好的运行资源。

修复：已移除上层按名称补偿；Docker 仅使用已创建容器 ID，K8s 仅对本操作 CM/Secret 使用 UID 条件删除；不删除 PVC。创建冲突保守报错，不自动接管赢家。

```rust
    async fn create_app_runtime(&self, app_id: &str, request: &CreateAppRequest) -> AppResult<()> {
        let params = self.build_container_params(app_id, request).await?;
        let container_info = self.runtime.create_deployment(params).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] create_deployment failed app_id={app_id}"),
                e,
            )
```

## R02 · P1 · 准备失败中断旧应用及恢复不完整

位置：[crates/app-cli/src/server.rs:396](/Users/soddy/Documents/git-workspace/rcoder/crates/app-cli/src/server.rs:396)

触发与后果：热部署遇到 404、坏包或启动失败时，过去可能先停旧服务或仍报告新版本成功。

修复：先 prepare 再停服务和 activate；编排失败恢复旧目录与旧进程；Recover 跳过迁移，明确报告恢复结果，绝不声称逆向回滚数据库。

```rust
async fn next_prepared(
    args: &CliArgs,
    state: &ServerState,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeployRequest>,
) -> Option<InitialAction> {
    loop {
        let request = rx.recv().await?;
```

## R03 · P1 · 并发受理及旧终态串代

位置：[crates/app-cli/src/server.rs:241](/Users/soddy/Documents/git-workspace/rcoder/crates/app-cli/src/server.rs:241)

触发与后果：并发部署或轮询撞上旧 Running 时，可能重复入队或把另一部署当成本次成功。

修复：同一 admission 锁完成检查、登记和入队；区分 operation_id、请求 release_id、manifest release_id；平台只接受匹配操作的终态。

```rust
    pub(crate) fn try_accept_deploy_with_id(
        &self,
        req: DeployRequest,
        operation_id: String,
    ) -> Result<(), String> {
        let _admission = self
            .admission
            .lock()
```

## R04 · P1 · 重复 env 更新丢失平台部署参数

位置：[crates/app_manager/src/lifecycle/start.rs:358](/Users/soddy/Documents/git-workspace/rcoder/crates/app_manager/src/lifecycle/start.rs:358)

触发与后果：start(url) 携带业务 env 时，后续重复更新可能覆盖部署种子/token 或再次重建容器。

修复：冷部署统一组装最终 env，去掉部署后的重复更新；hot 业务 env 有变化时在副作用前拒绝，相同 env 消去重复更新。

```rust
    async fn validate_hot_env(
        &self,
        app_id: &str,
        mut request: StartAppRequest,
    ) -> AppResult<StartAppRequest> {
        if request.deploy_mode == Some(DeployMode::Hot)
            && let Some(env) = &request.env
        {
```

## R05 · P1 · 注册表 miss 被当作权威不存在

位置：[crates/rcoder/src/userapp_forward/semantics.rs:59](/Users/soddy/Documents/git-workspace/rcoder/crates/rcoder/src/userapp_forward/semantics.rs:59)

触发与后果：请求落到未登记 builder 的副本时，取消/停止可能假成功，或只读请求错误触发创建。

修复：按服务类型和官方身份键只读查询 runtime，核对归属；查询错误透传，仅权威不存在允许幂等返回。

```rust
pub(super) async fn existing_dev_addr(
    state: &AppState,
    app_id: &str,
) -> Result<Option<String>, HttpResultError> {
    resolve_existing_dev(
        state
            .runtime()
            .find_container(app_id, &shared_types::ServiceType::UserappBuilder),
```

## R06 · P1 · 客户端校验版本不是真正 CAS

位置：[crates/docker_manager/src/runtime/k8s_app_create.rs:344](/Users/soddy/Documents/git-workspace/rcoder/crates/docker_manager/src/runtime/k8s_app_create.rs:344)

触发与后果：两个更新读到同一版本后仍可依次无条件覆盖配置，失败请求可能影响运行中的配置。

修复：配置使用操作独占名称；Deployment 写入携带 resourceVersion；提交失败只清本操作的 UID 凭据资源；响应丢失保留可能已被引用的配置。

```rust
            let commit = if expected.is_some() {
                api.replace(
                    &self.app_deployment_name(app_id),
                    &PostParams::default(),
                    &deployment,
                )
                .await
```

## R07 · P1 · SSE 订阅后的终态可能漏送

位置：[crates/file-server-userapp/src/handlers/userapp.rs:265](/Users/soddy/Documents/git-workspace/rcoder/crates/file-server-userapp/src/handlers/userapp.rs:265)

触发与后果：订阅创建后产生终态，而 handler 用之后的 live terminal 判断关流时，客户端可能收不到终态。

修复：回放、receiver、终态游标在同一快照内获得；只在请求游标已越过快照终态时直接关闭。

```rust
    let (replay, mut rx, terminal_seq) = task.subscribe_snapshot(from_seq).await;

    let progress = stream! {
        // 回放历史事件（seq >= from_seq）
        for (seq, ev) in replay {
            let terminal = is_terminal_event(&ev);
            yield Ok::<_, Infallible>(event_from_progress(seq, &ev));
```

## R08 · P1 · 取消与异步准备之间存在启动窗口

位置：[crates/file-server-userapp/src/service/userapp/tasks.rs:119](/Users/soddy/Documents/git-workspace/rcoder/crates/file-server-userapp/src/service/userapp/tasks.rs:119)

触发与后果：stop 在准备目录结束前返回时，旧异步任务之后仍可能启动或重启进程。

修复：应用级启动代次与提交保护串行化 stop/start；stop 先使旧代失效，旧代无法提交目录切换或进程启动；准备目录与进程提交共享同一代次边界。

```rust
    pub async fn commit_start<F, E>(
        &self,
        lifecycle: &Mutex<u64>,
        expected: u64,
        start: F,
    ) -> Result<bool, E>
    where
        F: Future<Output = Result<(), E>>,
```

## R09 · P2 · 下载/staging 残片与清扫竞争

位置：[crates/app-cli/src/deploy.rs:195](/Users/soddy/Documents/git-workspace/rcoder/crates/app-cli/src/deploy.rs:195)

触发与后果：取消期间 blocking 解压或开发 ZIP 准备失败可能留下 staging；并发清扫还可能删除活跃准备目录。

修复：操作独占 tempfile/TempDir；OS 准备租约覆盖 blocking 解压生命周期；启动只清理已结束操作的指定临时目录。开发运行目录也采用准备/激活分离及共享清扫租约。

```rust
        .context("create staging directory")?;
    // The OS lease outlives blocking extraction, including cancellation. Only a
    // holder can reclaim leftovers from a terminated operation.
    clean_owned_temporary(&incoming).await?;
    clean_owned_temporary(&staging_root).await?;
    let part = tempfile::Builder::new()
        .prefix("deploy-")
        .suffix(".part")
```

## R10 · 用户修订：取消 app-cli 容量与条目限制

用户确认由容器／存储环境管理容量，app-cli 不再主动限制下载字节数、解压总量、单文件大小或条目数量。原四个 `APP_DEPLOY_MAX_*` 配置不再读取；保留连接及读取空闲超时、SHA/ZIP/manifest 校验、路径与链接安全、操作临时目录清理。链接目标的路径合法性校验不属于制品容量配额。

回归已改为有效 B 制品超过旧测试限额仍须部署成功，并保留超时和安全故障必须失败的断言。以下全量验收表属于上一轮修复；本次定向证据另记于 Task。

## R11 · P1 · 正式 HTTP 错误信封与提取器不一致

位置：[crates/shared_types/src/userapp_http.rs:36](/Users/soddy/Documents/git-workspace/rcoder/crates/shared_types/src/userapp_http.rs:36)

触发与后果：参数提取失败或业务错误返回裸 4xx/5xx，调用方可能无法按统一 code 处理。

修复：正式业务入口统一 HTTP 200＋HttpResult、英文 message；同步 OpenAPI/三语错误资源；旧 TS、SSE 成功流、探针和应用字节流保持原协议。

```rust
pub async fn envelope_errors(request: Request, next: Next) -> Response {
    let formal = is_formal_path(request.uri().path());
    let response = next.run(request).await;
    if !formal || !(response.status().is_client_error() || response.status().is_server_error()) {
        return response;
    }
    let status = response.status();
    let fallback = if status.is_client_error() {
```

## 本地工具链配对问题

完整七服务链复现 Python 启动失败：缓存的 dev builder 基础镜像为 Python 3.11，制品含 `cpython-311-aarch64-linux-gnu.so`，runtime 为 Python 3.13，导致 pydantic_core 无法加载。通过 `AGENT_BASE_IMAGE` 显式使用已验证的 trixie 基础镜像重建 builder；不修改模板依赖或放宽启动断言。构建及 E2E 共用 `docker/verify-userapp-toolchains.py`，检查实际不可变镜像的 Python SOABI、架构和 Java 版本。旧配对实际返回非零，兼容基础镜像配对实际通过。

## standalone 制品链接修复

真实部署日志暴露第二个问题：聚合 ZIP raw-copy 丢失链接类型，生产/开发解压也缺少链接语义，Next 把链接目标当 JS 执行。已用显式 add_symlink 保留聚合元数据，并将链接路径、链与文件系统解析约束收敛到 shared_types::archive_links。保留内部依赖和安全悬空依赖；拒绝绝对/越界/循环链接及经已有链接写入；开发提取保留安全可执行位。两个组件红灯和双引擎 Docker 红灯均已留存；最终镜像上双引擎和真实七服务完整验收已通过，制品保留 57 个符号链接。

## 验证与边界

组件和协议证据见 [Task](tasks.md)，固定问题到用例映射见 [Spec](spec.md)。最终命令结果如下；候选和早期失败 run 仅保留为修复过程证据。

|验证|实际结果|
|---|---|
|workspace fmt、默认 Clippy、Kubernetes 特性 Clippy|通过|
|`cargo test --workspace --locked`|1835 passed、0 failed、35 ignored；环境门控忽略不计入 E2E|
|app-cli 独立 Cargo.lock：fmt、Clippy、test|通过，99 passed|
|K8s 实际客户端／本机 API 适配器|3 passed：版本竞争、并发创建、提交响应丢失|
|测试入口工具回归|9 passed|
|builder/runtime 镜像构建及 `make dev-hot`|通过；实际运行二进制 SHA256 与编译产物相同|
|[严格 userApp 报告](../../tests-e2e/reports/f24f39d55adb4987b330da992ae9f192/summary.json)|31/31 passed、378 项硬断言、33 条清理记录成功|
|[Compose 报告](../../tests-e2e/reports/474a9b4e5a304589bb76bf4d8a05f64b/summary.json)|54/54 passed、317 项硬断言、58 条清理记录成功|

两个完整 run 均退出 0，无跳过、中止、缺报告、源码漂移和清理错误；使用同一工作区指纹 `2f5c78d8d7c723e12ae03c2b88467492f2f0c7bb0174d845dbb1dce4d38e3c2b`。验收后仅补充文档证据。容器集合与验收前完全一致，既有用户资源无丢失。

[产物身份与归档日志](../../tests-e2e/reports/userapp-repair-evidence/artifacts.json)：builder `01ef2f773f2a…`，runtime `ff40b26fa8f3…`，主程序 `c7d0284e4a0f…`。builder/runtime 实测 Python SOABI、架构和 Java 版本匹配。主容器运行 dev-hot 二进制；主容器镜像本身没有重新构建。

测试入口新增严格选择、每次唯一 run、每进程报告、必需步骤清单、来源冻结、容器身份追溯、失败诊断和定向清理；使用方式见 [测试入口说明](../../tests-e2e/tools/README.md)。两套入口均保留真实 AI 调用。

- 已排除：SSE emit 的 broadcast 已在 state 锁内，不存在本次怀疑的发布乱序；修的是关流快照。
- 已排除：runtime trait 默认方法不能代表实际实现；Docker/K8s 的真实覆写已按调用链核查。
- 已排除：已修复的类型 label 兜底、注册表 cross_verify、ZIP 魔数及四槽位身份不重复列为新发现。
- 实现取舍：创建凭据由 runtime 内部持有并执行补偿，不向上层返回用于二次补偿；创建冲突保守报错，未自动重查接管赢家。此选择避免错误复用或误删，但与原计划的自动重查复用步骤不同。
- 保留边界：Docker 不提供 K8s resourceVersion CAS；现有 Docker 热部署后按创建 env 种子重启的限制仍存在。K8s 实机多副本与故障恢复由后续部署验证，本轮没有连接或操作集群。
- 回归流程并非每项都留有修改前的红灯记录；保留了 SSE、真实 Docker 启动误报及重复迁移的红灯，最终通过证据见上表。

架构上继续保持三层职责：app_manager 协调部署，runtime 负责资源条件写入，app-cli 负责单次部署和进程编排；跨 crate 的条件更新与部署状态契约收敛到 shared_types。若后续要求跨副本、跨重启的操作线性一致性，应另立持久化操作记录与完成确认协议，不能靠探活或本地内存锁推断。
