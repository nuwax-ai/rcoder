# UserApp 差距清单(2026-08)

> 本文记录对 UserApp「发布 / 运行时 / 日志」链路做设计↔实现↔文档交叉核查后发现的待修项。每条含
> 证据、触发条件、影响、**验证方法**、建议修复和验收标准,供独立复核与开发修复。
>
> **核查分支**:`feature-pod-ceph-file`(2026-08)。核查方式:并行通读 rcoder 代码 + 对照
> `application-management-service-v2-design.md`。
>
> **优先级**:🔴 高(正确性/数据) · 🟠 中(UX/健壮性) · 🔵 低(TODO) · 🟢 文档同步
>
> **修复状态(2026-08)**:R1-R4、U1-U3、T1、P1 已全部实施完成并通过单测/clippy 验证;T1 采用
> Pingap admin loopback 只读确认方案(admin 仅用于 reload 生效确认,不经 admin 修改配置)。
>
> **重要前置结论(不是问题,别误改)**:
> - **per-app 子域名访问由独立的前端/网关项目实现**(host→app 映射后转发 rcoder),rcoder 后端是
>   Pingora 路径代理 `/proxy/apps/{app_id}/{port}/`(无 hostnames)——这是设计分工,不是缺口。
> - **R4(disabled 服务)当前不触发功能 bug**:build 阶段已把 `enabled=false` 排除出 release.lock,
>   `LockedService.enabled` 恒为 true。R4 是"防御性深度"债务。

---

## 🔴 R1 — 首次发布失败:code 被删空 + 空壳 Deployment 残留(✅ 已修复)

### 问题
发布编排的失败补偿(post_activate 收敛到 `confirm_release(healthy=false)`)对**升级**场景完善
(有旧 code 可回滚),但对**首次发布**不成立:首次发布没有旧 code,失败时不仅回滚不出东西,
还把刚切进来的新 code 删了,且不清理已建的 Deployment/K8s 资源,留下"空壳"。

### 证据
- `crates/rcoder/src/userapp_publish/orchestrator.rs:193-264` —— activate 之后的所有阶段
  (EnsureApp/WaitReady/Confirm)包进 `post_activate`,任何 Err 收敛到 `confirm_release(healthy=false)`。
  注释明写"activate 之后的所有退出路径都必须收敛到 confirm,否则永久留下 pending_release_id"。
- `crates/app_manager/src/releases.rs:322-372`(`confirm_release` 的 healthy=false 分支):
  - `:330-337` 先 `stop_app`(= scale_to_zero,**不删 Deployment**,见 `app_ops.rs:52-54`)。
    若 Deployment 还没建,`stop_app` 返 NotFound,被 `Ok(_) | Err(NotFound) => {}` 吞掉。
  - `:338` `remove_dir_if_exists(&code)` —— **删掉刚切进来的新 code**。
  - `:340` `if rollback.exists()` —— **首次发布 rollback 不存在 → 跳过恢复**。
- `crates/app_manager/src/releases.rs:190-191,214` —— activate 里 `if code.exists()` 才把 code rename
  到 rollback;首次发布 `code` 不存在,整个 if 跳过,所以 rollback 始终不存在。
- `crates/rcoder/src/userapp_publish/app_lifecycle.rs:89-120` —— `wait_ready` 超时(默认 600s)或
  app 进 Error 都走 confirm(false)。
- `crates/app_manager/src/service.rs:125-136` —— `create_app`(首次发布 ensure_app 走它)失败直接
  return Err,**无清理**。

### 触发条件
首次发布(应用此前无任何 release),且下列任一在 activate 之后发生:
1. `create_app`(建 PVC/目录/Deployment)失败;
2. `wait_ready` 600s 超时(app 启动慢、健康端点配错、app crash);
3. 健康确认最终 unhealthy。

### 影响
- `/app/code` 被删空;
- Deployment 若已建则残留(replicas=0,空壳);K8s 模式下 create_deployment 部分成功产生的
  ConfigMap/Secret/Service 也残留且无清理(见 R2);
- 该应用在下次成功发布前持续不可用;流量唤醒/手动 start 会 crash-loop;
- 与用户"失败自动回滚"的心智不符(首次发布前没有可回滚的版本)。

### 验证方法(给复核 agent)
1. 读 `releases.rs:322-372` 确认 healthy=false 分支在 `rollback.exists()==false` 时只 `remove_dir(code)`
   而不删 Deployment;
2. 读 `orchestrator.rs:193-264` 确认首次发布的 ensure_app/wait_ready 失败都进这条分支;
3. **构造复现**:对一个全新 app_id 发一次必然失败的发布(例如 manifest 的 `[health].readiness_path`
   指向一个不存在的路径,使 wait_ready 超时),发布 Failed 后查:`/app/code` 是否为空/不存在、
   `kubectl get deploy` 是否残留该 app 的 Deployment。预期:code 空 + Deployment 残留 = 复现。

### 建议修复
在 `confirm_release(healthy=false)` 路径(`releases.rs:322-372`)识别"首次发布"(等价于
`!rollback.exists()` **且** 本次发布创建了计算资源),此时应 `delete_app`(删 Deployment 及关联
K8s 资源)而不是只 `stop_app`,让应用回到"未部署"的干净态,而非空壳态。

具体:`releases.rs` 的 confirm(false) 分支,把 `stop_app` 换成判断——若 `!rollback.exists()` 则
`delete_app`(彻底回收计算资源),否则维持现有 stop + 回滚旧 code 逻辑。

### 验收
首次发布失败后:`/app/code` 不残留空目录、`kubectl get deploy` 无该 app 的 Deployment、release
index 标 Failed 且无 pending。下次发布能正常成功。

---

## 🔴 R2 — create_app 部分失败无回滚(✅ 已修复)

### 问题
`create_app` 内部多步(建 PVC/目录 → create_deployment + 注册 pingora),任一步失败直接 return Err,
**不清理已成功的步骤**;编排层也不调 `delete_app` 兜底。

### 证据
- `crates/app_manager/src/service.rs:241-258` —— `create_app_runtime` 顺序建资源,失败即 return,
  无 try/catch 式回滚;
- K8s 模式 `create_deployment` 可能先建了 ConfigMap/Secret/Service 再失败(或反之),半成品全残留;
- `orchestrator.rs` 的 ensure_app 失败路径只走 confirm(false),不显式 delete_app(R1 同一根因)。

### 触发条件
首次发布或 create_app 任一子步骤失败(配额不足、K8s 瞬时错误、镜像拉不下来等)。

### 影响
半成品资源(PVC、目录、ConfigMap、Secret、Service、部分 Deployment)残留,占配额、影响下次创建
(同名冲突或脏状态)。

### 验证方法
1. 读 `service.rs:241-258` 确认各步失败无清理;
2. 复现:让 `create_deployment` 在建完 Service 后失败(例如给一个非法 image 引用或蓄意打 patch 失败),
   查 Service/ConfigMap 是否残留。

### 建议修复
`create_app_runtime` 失败时内部做 best-effort 回滚(删已建资源);或在 ensure_app 失败分支显式
调 `delete_app` 兜底(与 R1 的修复同处)。

### 验收
create_app 任一步失败后,不残留半成品资源;下次同名 create 能干净成功。

---

## 🔴 R3 — confirm(healthy=false) 自身失败 → pending 卡死(✅ 已修复)

### 问题
post_activate 的失败收敛依赖 `confirm_release(healthy=false)` 成功清掉 pending_release_id。若
confirm 自身失败(文件锁竞争、index 写磁盘错误),pending 没清,下次发布的 activate 被守卫拒绝,
卡死需人工介入。

### 证据
- `crates/rcoder/src/userapp_publish/orchestrator.rs:248-261` —— confirm(false) 再失败时只 emit
  Failed(合并错误信息),**不强制清 pending**;
- `crates/app_manager/src/releases.rs:172-176` —— activate 的 `pending_release_id` 守卫:
  已有 pending 则返回 `InvalidState("release X is still pending confirmation")`;
- `releases.rs:309-321` —— confirm 内部对"pending≠自己/状态非 Failed"返回 InvalidState。

### 触发条件
confirm(false) 写 index / 持文件锁时失败(磁盘压力、锁竞争)。**概率低但真实。**

### 影响
该 app 无法再发布(activate 全被 pending 守卫挡),必须人工 `delete_release` 或直接改 PVC 上的
index.json 清 pending。

### 验证方法
1. 读 `orchestrator.rs:248-261` 确认 confirm(false) 失败路径不清 pending;
2. 读 `releases.rs:172-176` 确认 pending 守卫会挡后续 activate;
3. 可在测试里 mock confirm_release 返 Err,验证 pending_release_id 残留 + 下次 activate 被拒。

### 建议修复
orchestrator 检测到 confirm(false) 失败时,强制清 pending(直接写 index 或新增 `abort_release`
接口,绕过正常 confirm 路径强制释放 pending)。或给 release 增加一个"force clear pending"的
运维接口。

### 验收
confirm(false) 失败后,pending 被强制清掉,下次发布能正常 activate。

---

## 🟠 R4 — disabled 服务:supervisor 与 Pingap 生成缺防御性 `.filter(enabled)`(✅ 已修复)

### 问题
`enabled=false` 的服务在 build/release_lock 阶段已被排除(不进 release.lock),所以当前**无功能
bug**。但运行时消费者(app-cli supervisor 的 migrate/start、managed Pingap 路由生成)**没有自己
判断 enabled**,完全靠 build 兜底。一旦 release.lock 含 disabled(手动编辑、或未来按
`LockedService.enabled` 字段设计意图改成 inclusive 锁),disabled 服务会被 migrate + start、并被
生成 pingap 路由(用未分配的端口 → 无效地址)。

### 证据
- **build 正确跳过**:`crates/file-server/src/service/userapp/mod.rs:103-106` `.filter(enabled)`;
- **release_lock 正确跳过**:`crates/workspace-manifest/src/release_lock.rs:89-92`(allocate_ports)、
  `validation.rs:100-103`(validate_topology 返回值只含 enabled id);
- **`LockedService.enabled` 恒为 true(死字段)**:`types.rs:238` 有该字段,但因 build 已过滤,
  进 lock 的服务全是 enabled,字段退化;
- **supervisor 不 filter(债)**:`crates/app-cli/src/supervisor.rs:27` `specs = release.services.clone()`,
  `:34` `for spec in &specs`,`:37-44` migrate 只判 `!spec.run.migrate.is_empty()`,`:46-56` start
  只判 `!spec.run.command.is_empty()`——**均无 enabled 检查**;
- **managed Pingap 生成不 filter(债)**:`crates/app-cli/src/proxy/compiler.rs:62-78` `managed_config`
  用 `filter_map(|service| service.proxy.as_ref()...)` 只过滤 proxy=None(Worker),**不检查 enabled**;
  `:96` `compile_extend` 同样 `for service in &release.services` 不过滤;
- **注意**:`compiler.rs:150-156` `resolve_service_addresses` **有** `.filter(|service| service.enabled)`,
  但它只解析 `rcoder://` URI(custom 模式),而 managed 生成的 upstream 是 `127.0.0.1:{port}`
  (`proxy/pingap.rs:53`),**不经此路径** → 对 managed 模式这道 filter 是 no-op,挡不住 disabled;
- **代码库先例**:`crates/app-cli/src/log/service.rs:172-176` 的日志查询**有** `.filter(enabled)` ——
  说明 enabled 过滤是既有约定,supervisor/pingap 没对齐。

### 触发条件
当前分支:**不触发**(build 已排除 disabled)。触发需要 release.lock 含 `enabled=false` 的 service:
手动编辑 `/app/code/release.lock.toml`,或未来把锁语义改成 inclusive(让 `LockedService.enabled`
字段名副其实)。

### 影响
当前无。潜在:一旦锁含 disabled,会 migrate/start 不该启的服务、为 disabled 服务生成指向未分配
端口的 pingap 路由(502)。

### 验证方法
1. 读上述 supervisor.rs / compiler.rs 确认无 enabled 过滤;
2. 对比 log/service.rs:172 确认有先例;
3. 复现(可选):手动造一个含 `enabled=false` service 的 release.lock.toml 放进容器,启动 app-cli,
   观察该 service 是否被 migrate/start、是否生成路由。

### 建议修复(二选一)
- **方案 A(推荐,低成本,对齐设计 §12.2.9)**:`supervisor.rs:34` 改为
  `for spec in specs.iter().filter(|s| s.enabled)`;`compiler.rs:62-78` 的 managed_config 和 `:96`
  的 compile_extend 迭代前加 `.filter(|service| service.enabled)`。
- **方案 B(设计变更)**:明确锁语义为 exclusive,移除 `LockedService.enabled` 死字段
  (`types.rs:238`)。但这涉及 manifest 演进,需评估。

建议先做 A(纯防御性,零行为变化)。

### 验收
代码里 supervisor migrate/start 和 managed Pingap 生成对 `enabled=false` 显式跳过;即使 release.lock
含 disabled 也不启动它、不为它生成路由。

---

## 🟠 U1 — runtime 日志源(stdout/stderr)不自动注册(✅ 已修复)

### 问题
app-cli 把每个服务的 stdout/stderr 捕获落盘成 `runtime.out.log`/`runtime.err.log`,但**不**在日志
查询接口自动注册一个名为 `runtime` 的 source。用户若没在 manifest 声明匹配 `runtime.*.log` 的
`[[logs.sources]]`,在日志面板就看不到服务 stdout——"服务起不来"时第一手排查信息丢失。

### 证据
- `crates/app-cli/src/supervisor.rs:226-227,248-259` —— 写盘 `runtime.out.log`/`runtime.err.log`,
  但无注册动作;
- `crates/app-cli/src/log/service.rs:172-190` —— 查询的 source 列表**完全来自** release.lock 的
  `services[].logs`(即 manifest 声明),无额外注入;
- `crates/workspace-manifest/src/types.rs:171-179` —— `LogSource` 是 `deny_unknown_fields`,
  无默认 source;
- 全 crate grep:生产代码无 `LogSource { ... }` 合成注入(仅 `log/service.rs:696,738` 在 `#[cfg(test)]`)。

### 触发条件
**每个**没在 manifest 声明 `runtime` source 的服务(模板里的前端是手动声明了 `id="runtime"` 才看到)。

### 影响
用户不声明就看不到 stdout/stderr,排查启动失败困难。与设计文档 §12.2.8("明确 runtime.*.log 是否
自动注册为默认 source")一致——这是个悬而未决的 TODO。

### 验证方法
1. 读 `supervisor.rs:226` 确认写盘但无注册;读 `log/service.rs:172-190` 确认只看 manifest;
2. 复现:部署一个 `[[logs.sources]]` 为空的服务,让它 `echo` 到 stderr,在日志面板查——查不到。

### 建议修复
app-cli 在加载 release.lock 后,为每个 service **默认注入**一个 `runtime` source(若该 service 未
声明同 id 的 source):
`LogSource { id: "runtime", glob: "runtime.*.log", format: LogFormat::Text, multiline_start_pattern: None }`。
注入位置:`LogService::new` 或 `select()` 取 source 前,对 `service.logs` 做合成补全(仅内存,不写回
release.lock)。

### 验收
manifest 不声明任何 `[[logs.sources]]` 的服务,其 stdout/stderr 也能在日志面板(选 `runtime` source)
看到。

---

## 🟠 U2 — 同 app 并发 publish 无早拒绝(✅ 已修复)

### 问题
`PublishTaskStore::create` 只查全局容量(默认 1000),不查"该 app 是否已有进行中的 publish 任务"。
同一 app 两次并发 publish 都被接受、各自 spawn、并行跑完整 build+prepare,直到第二个在 activate 撞
pending 守卫才失败——浪费构建资源、UX 差。

### 证据
- `crates/rcoder/src/userapp_publish/store.rs:42-70` —— `create` 只检查全局活跃任务上限,无 per-app 检查;
- `crates/rcoder/src/userapp_publish/handler.rs:148-173` —— publish handler 直接 `create() + tokio::spawn`;
- 实际防护靠 app_manager 层:`releases.rs:172-176` 的 `pending_release_id` 守卫(只在 activate 生效)
  + `service.rs:50` 的 per-app 异步锁 + `.operation.lock` 文件锁。

### 触发条件
对同一 app_id 几乎同时发两次 publish。

### 影响
正确性没问题(pending 守卫保住),但第二个任务白跑 build+prepare(占构建容器资源、占全局任务额度),
且用户要等很久才在 activate 阶段拿到失败。

### 验证方法
1. 读 `store.rs:42-70` 确认无 per-app active 检查;
2. 复现:同一 app 连发两次 publish,观察两个任务都进 building/prepare,第二个最终在 activate 失败。

### 建议修复
`PublishTaskStore::create` 加 per-app active task 检查:扫描现有任务,若该 app_id 已有
Pending/Running/Cancelling 的 publish 任务,返回 `Conflict`;handler 返回 **409**(fail-fast)。

### 验收
同 app 已有进行中 publish 时,第二次 publish 立即 409,不进入 build。

---

## 🟠 U3 — `POSTGRES_*`/`PG*` 未进保留环境变量清单(footgun)(✅ 已修复)

### 问题
容器继承的 `POSTGRES_USER`/`POSTGRES_DB`/`POSTGRES_PASSWORD`/`PGHOST`/`PGPORT` 没在 manifest 的
保留 env 清单里。用户若在 `[env]` 覆盖(如 `POSTGRES_PASSWORD = "xxx"`),校验放过,但服务进程读到
的密码会和 PG 实际初始化用的密码错开 → 静默连不上库。

### 证据
- `crates/workspace-manifest/src/validation.rs:282-287` —— `is_reserved_env` 只保留
  `PORT/HOST/HOSTNAME/APP_LOG_DIR/APP_SERVICE_ID/APP_RELEASE_ID` 和 `RCODER_*` 前缀;
- `crates/app-cli/src/supervisor.rs:232` —— `.envs(&spec.env)` 在继承父进程 env 基础上追加 `[env]`,
  即 `[env]` 的同名 key 会**覆盖**继承的容器 env;
- `crates/app-cli/src/supervisor.rs:152-155` —— app-cli 读 `POSTGRES_*` 用于自己的 pg_isready;
- `build-agent-docker/build_config/app-runtime-base/pg-supervisor-entry.sh` —— PG 用容器的
  `POSTGRES_PASSWORD` 做 initdb(`--pwfile`),所以 PG 实际密码 = 容器 env 的值,不会被 `[env]` 改。

### 触发条件
用户在 `project.manifest.toml` 的 `[env]` 里写 `POSTGRES_PASSWORD`(或 `POSTGRES_USER`/`POSTGRES_DB`/
`PGHOST`/`PGPORT`)。

### 影响
服务用 `[env]` 的值去连,PG 用容器 env 的值设的密码 → 认证失败,服务连不上库,且报错不直观
(用户以为自己配对了密码)。

### 验证方法
1. 读 `validation.rs:282-287` 确认 `POSTGRES_*`/`PG*` 不在保留清单;
2. 复现:在 `[env]` 设 `POSTGRES_PASSWORD = "wrong"`,部署后服务连 PG 报认证失败,但用 `app`/容器
   实际密码能连。

### 建议修复
`is_reserved_env` 把 `POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB`/`PGHOST`/`PGPORT` 加入保留
(返回 true),让 `[env]` 出现这些 key 时构建直接失败、报清楚原因。同时在模板文档强调这些变量直接读、
别覆盖(文档侧已加)。

### 验收
`[env]` 出现上述 key 时 manifest 校验失败,报"reserved by runtime"。

---

## 🔵 T1 — Pingap reload 无生效确认(设计 §12.2.6,TODO)(✅ 已修复)

### 问题
`POST /v1/proxy/reload`(及 app-cli 的 Pingap 配置编译)只做 `pingap -t` 语法校验 + 原子落盘,就
返回"已重载"。**不**确认 Pingap 进程真的加载了新配置、无 config hash 比对、无失败回切。Pingap 靠
`--autoreload` 自轮询文件变动重载,app-cli 不感知重载结果。

### 证据
- `crates/app-cli/src/api/proxy.rs:33-46` —— `reload()` 仅调 `compile()`,成功就返
  `{"reloaded": true, ...}`;
- `crates/app-cli/src/proxy/compiler.rs:43-58` —— `compile_and_validate` 的"验证"只有
  `pingap -t -c <tmp>`(dry-run),通过后 `tokio::fs::rename` 落盘返回;
- `crates/app-cli/src/supervisor.rs:333` —— Pingap 以 `--autoreload` 启动,自行轮询重载。

### 触发条件
任何触发 Pingap reload 的操作(custom/extend 配置变更、reload API 调用)。

### 影响
配置写成功但 Pingap 进程因故没加载(文件权限、autoreload 抖动等),平台却以为生效,实际路由仍是旧的。

### 验证方法
1. 读 `compiler.rs:43-58` 确认只有 `pingap -t` + rename,无进程级确认;
2. 读 `proxy.rs:33-46` 确认 reload 直接返成功。

### 建议修复
reload 后增加生效确认:校验 Pingap 进程加载的 config hash(例如读 `/v1/proxy/effective-config`
比对 hash,或检查 Pingap admin/状态接口确认 reload 完成),失败则回切上一份配置并报错。设计 §12.2.6
已列为 TODO。

### 验收
reload 返回成功时,Pingap 实际加载的配置与写入一致;加载失败时 reload 报错并回切。

---

## 🟢 P1 — cursor_reset 事件已实现(设计 §12.2.10 标 TODO,实际已落地)(✅ 已修复)

### 问题(文档/代码不同步)
设计文档 `application-management-service-v2-design.md` §12.2.10(item 10)把"app-cli 重启后 boot ID
变化应显式发送 cursor_reset"列为 TODO,但**代码里已实现**。

### 证据
- `crates/app-cli/src/log/service.rs:47` —— `boot_id` 每次 `LogService::new` 重新生成(每次启动都换);
- `crates/app-cli/src/log/service.rs:401-404` —— `decode_cursor` 检测旧 boot_id 返回 `cursor_reset=true`;
- `crates/app-cli/src/api/mod.rs:206-211` —— SSE handler 在 `response.cursor_reset` 为真时**显式 yield**
  `cursor_reset` 事件(`api/mod.rs:243-249` query 出错也发)。

### 建议处理
把设计文档 §12.2.10 该条标注为"已完成",避免后续误当作待办重复实现。

### 验收
设计文档该条状态更新;代码行为不变。

---

## 附:核查中确认正确、无需改动的部分

- **升级场景的 activate 内部失败补偿**(`releases.rs:207-283`)四条失败分支都有"清 pending + 还原 +
  重启旧"补偿,扎实。
- **post_activate 收敛线**(`orchestrator.rs:191-264`):activate 后任何失败都调 confirm(false),
  不泄漏 pending(除 R3)。
- **cancel 状态机**(`task.rs:143-158`):原子转 Cancelling(非终态),终态由 orchestrator 收敛,
  设计正确。
- **端口分配**(`release_lock.rs:86-120`):service_id 哈希 + 跨输入顺序稳定,保留端口 5432/7681
  正确避让。
- **manifest 严格校验**(`validation.rs`):DNS-1123/argv/路径/路由唯一/catch-all 唯一/worker 禁 proxy/
  依赖环/保留 env,均正确。

---

## 修复建议优先级

| 优先级 | 项 | 类型 | 工作量 |
|---|---|---|---|
| 1 | R1 + R2 | 首次发布失败清理 | 中(改 confirm(false) + create_app 回滚) |
| 2 | U1 | runtime source 默认注入 | 小(app-cli 注入合成 source) |
| 3 | U3 | POSTGRES_* 加入保留 env | 极小(is_reserved_env 加几行) |
| 4 | R4 | disabled 防御性 filter | 小(两处加 .filter) |
| 5 | U2 | 并发 publish 早拒绝 | 小(store 加 per-app 检查) |
| 6 | R3 | confirm(false) 失败强清 pending | 中 |
| 7 | T1 | Pingap reload 生效确认 | 中(需 Pingap 状态确认机制) |
| 8 | P1 | 设计文档同步 | 极小(改文档) |
