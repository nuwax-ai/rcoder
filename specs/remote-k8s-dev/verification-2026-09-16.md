# 远端 amd64 构建与 K8s 验证记录（2026-09-16）

## 本轮结论

已实测通过 Mac 源码同步、Linux amd64 编译、三镜像推送、K8s 按 digest 部署及 smoke。可以使用这条工作流做远端开发验证。

Gateway 实际 HTTP 请求失败；因此不能宣称完整 K8s 功能验收通过。UserApp/Chat 全链路本轮未执行，nextest 不在本工作流的远端构建步骤中。

目标主机、context、namespace、registry 和访问地址均复用 `.env.local`；使用 SSH 密钥，无需写入密码。本轮只操作既有专属环境，未修改共享 Gateway/Cilium/节点网络配置，未删除 PVC。

## 命令与结果

| 命令 | 退出码 | 实际范围 |
|---|---|---|
| `make remote-k8s-doctor` | 0 | Mutagen 0.18.1、SSH、x86_64、Buildx、Ready amd64 节点、存储类、CRD 和 registry 可访问 |
| `python3 -m unittest discover -s tools/remote_k8s/tests -v` | 0 | 11 项工作流测试通过 |
| 首次 `make remote-k8s-verify SUITE=smoke` | 2（Make） | 同步配置指纹变化，正确拒绝复用旧会话，尚未进入构建 |
| `make remote-k8s-sync-stop` / `make remote-k8s-sync-start` | 0 | 按使用说明重建本项目的同步会话 |
| 再次 `make remote-k8s-verify SUITE=smoke` | 0 | 三镜像构建/推送、部署、双副本 Ready、四个 PVC Bound、直连健康接口；环境外资源检查通过 |
| `make remote-k8s-test SUITE=gateway` | 2（Make） | Gateway 健康 HTTP 请求超时，严格报告 fail |
| 独立重试 `make remote-k8s-logs` | 0 | 采集到 Pod、事件和部分日志；其中一个 RCoder 副本的文件日志未生成，不能视为全部诊断项成功 |

## 构建身份

- build_id：`20260916T023335Z-5951e882`
- source_sha256：`84c7d8a15fb214a42dc999cc1d11fdcdc45adfa0118778438788a296e355ae3a`
- 冻结文件数：1490；来自当时含未提交开发修改的工作树，不等于仅 HEAD。
- 三镜像来自同一远端独立复制快照；后续本地修改不进入该快照。
- rcoder digest：`sha256:cd4636ecd64c5fe0cc11015d13d564d63d128e5912741c851f5dd61adc046513`
- computer digest：`sha256:7a971ad7d284d1e625b65c5bd95b973d7dfb3f1d6a9fec5ee9a4b6312bb4cc6c`
- runtime digest：`sha256:b376b7b61538a2de97ed978170372abeb7049fb0b31b27551e447e5f237181b5`
- 编译有 app-cli unused import/mut 警告及 Dockerfile ARG 默认值警告，但构建退出成功；本轮没有修改业务源码消除警告。

证据位于本地忽略目录：

- 构建记录：`.remote-k8s/c188d7e8de407557/builds/20260916T023335Z-5951e882.json`
- 输入文件指纹：`.remote-k8s/c188d7e8de407557/builds/20260916T023335Z-5951e882-source.json`
- 各镜像构建日志：同目录同 build_id 的 `*-rcoder.log`、`*-computer.log`、`*-runtime.log`
- 部署记录：`.remote-k8s/c188d7e8de407557/deployment.json`
- smoke：`.remote-k8s/c188d7e8de407557/tests/4ae12a62c96d4caaa2d575a16e3c2e72/summary.json`
- gateway：`.remote-k8s/c188d7e8de407557/tests/014fb03c0b9a427bb1fce477ebeea54c/summary.json`
- ClusterIP 对比：同 gateway 目录 `cluster-connectivity.json`
- 独立诊断重试：`.remote-k8s/c188d7e8de407557/logs/9c286b5281404581b8bf668e66d32f94/`

## Gateway 与集群连接问题

本轮实际观测：

- Mac → RCoder 直连 NodePort `/health`：HTTP 200。
- Linux → RCoder 直连 NodePort `/health`：HTTP 200。
- Mac/Linux → Gateway NodePort `/health`：5 秒 curl 超时。
- Linux → RCoder Service ClusterIP `/health`：HTTP 200。
- Linux → Gateway Service ClusterIP `/health`：5 秒 curl 超时。
- Gateway Accepted/Programmed=True；HTTPRoute Accepted/ResolvedRefs=True；路由指向本环境 RCoder Service。

由此可确定：镜像运行和直连服务健康，Gateway 状态条件不足以证明转发可用。故障位于 Gateway 访问/转发链路，具体 Cilium/Envoy/网络根因未确定；不能直接归因为业务代码或某项集群配置。

Gateway 失败后的自动诊断也返回 RuntimeError；独立查询节点时复现 `net/http: TLS handshake timeout`。需分别排查控制面间歇性连接和 Gateway 数据路径，不能把两者未经证明归为同一根因。

## 工作流边界与建议

1. `remote-k8s.mk` 是 Python 工作流的入口，命令映射本轮可用，无需为了此次验证改写 Makefile。
2. 构建在 Linux 上执行 Cargo/Buildx；测试由 Mac 启动，向远端 K8s 请求。当前没有“远端 nextest”阶段，不应把编译成功称为 Rust 测试通过。
3. `verify` 会构建并部署新源码；`test` 只对已部署镜像追加验证，务必区分。
4. 继续使用不可变快照和 digest，避免边编辑边构建产生混合源码。userapp/chat/all 还会检查本地测试源码漂移，应冻结测试源码后执行；smoke 不能替代这些业务验收。
5. 同步规则变化时先 sync-stop/sync-start；当前机制是明确拒绝，不是自动更新旧会话。
6. 后续可改善 diagnostics：各采集项独立记录失败、保留具体错误，避免首个控制面错误导致整份诊断缺失。不要用放宽测试或绕过 Gateway 使全链报告通过。

本轮保留专属部署、镜像回执和同步会话，便于继续排查。测试环境没有执行 down，数据卷保留。

---

# 第四轮（2026-09-16 深夜）：锁快失败 E2E 收口 + 第四轮审查 Q/K/R/F 修复批

本轮基线：`092c05dc`（第四轮审查基线）之上的增量提交 `94550967 → e419e2ba`（本轮共 13 个提交，见下）。上文历史报告保持原样。

## 一、源码基线与本轮修复清单

| 提交 | 内容 |
|---|---|
| 94550967 | restart OpenAPI 补锁占用信封说明（第三轮遗漏项） |
| b6084f06 | T03/T04/T05/T06/T07 工具验收可信度（首轮修复） |
| 53d2280b | F01 技能清单路径穿越校验（单 Normal 路径段） |
| 092c05dc | 并行任务收编（AGENTS/Make 规范 + Dockerfile chmod，含本轮在途改动） |
| b7cd7e8e | Q01/Q02 规范形态不变式 + 受管根链接防护 |
| 66d8d477 | K01 空快照完成候选 / K02 写后冲突保护 / K03 UTF-8 截断 |
| 898fe824 | Q03 缓存真禁用+digest 入构建 / Q04 retest 收束复用 / Q05 全路径末校验 / Q06 诊断 / T04 结构化 |
| fac179ea | R01-R06 app-cli 执行身份/终态单调/提交原子化/取消收束/恢复门控/清理顺序 |
| 4a58deab | F04 TempDir 存活 + .md 目标分派 + 源丢失报错 |
| c3b8aaaf | harness 清理重试预算对齐 1800s |
| 71ba1791 | Gateway 路由 8088 代理族 + 冻结快照禁字节码 |
| e9406189 | Service 补 8088 口 |
| a501240c→8a40656b→57c6b93f→a9fb82b3 | 锁场景 harness 迭代（静默等待/断言契约对齐/线程收束次序） |
| 00913288 | **产品修复**：K8s 模式租约等待语义（wait=true 轮询）+ Mock 对齐 |
| e419e2ba | retest 包装层 KeyError |

第四轮审查逐项状态：Q01-Q06 全部修复+回归；K01-K03 修复+回归；R01-R06 修复+回归；F04 修复+回归；**未完成**：F02/F03（resolved context 贯穿）、F05（退役字段文档复验）、Q07（视图锁所有权）、R07/R08、K04/K05——按审查排序属第三批，留待下一轮，不因测试通过视为已解决。

## 二、远端 K8s 验收（131 个人环境，env c188d7e8de407557）

最终轮（链第 10 轮）：`make remote-k8s-verify SUITE=smoke` → 构建（源 21bf51b7 系）→ 部署 uid/generation 见 deployment.json → smoke pass；随后 `remote-k8s-test`：

- **gateway**：pass（报告 tests/9270b0b8… 系最终轮目录）
- **userapp**：**pass，52/52 断言全绿**（报告 tests/b2eed6d2cfa845fabcc7be1fdee13126）——含本轮新增锁真实场景：
  - 忙锁窗口（热部署在途、受理记录为可观察同步点）：stop 同实例/跨副本、restart、delete、start（HTTP 面）全部 HTTP 200 + ERR_CONFLICT + success=false 立即返回（耗时记录于 lock-failfast.json/requests.json）；持有者仍在途（不等待）；释放后无自动执行（pod Running、无 Succeeded Stop 记录、内容不变）
  - delete 乐观锁：stop/wake 换代后过期 resource_version 被拒，Deployment+PVC 保留
  - 跨副本锁：entries[] 逐副本 port-forward 直连证明 ConfigMap 租约互斥
- **chat**：2/3（lb_entry_rotation、lb_cross_entry_cursor_reconnect pass）；lb_new_session_cross_entry 轮 2 收 0 事件失败——重跑两次均 pass（retest 路径实机验证：唯一报告目录、父报告源码绑定、完整收束复用），归因真实 LLM 抖动，非回归

锁竞争响应耗时：各冲突探测 elapsed_ms 见 userapp 报告 requests.json（数量级 <1s，远小于持有者部署时长）；冲突信封样例（live）：
`{"code":"ERR_CONFLICT","success":false,"message":"application operation is in progress"}`（同实例）与 `"acquire application operation: Application operation in progress: rcoder-operation-prod-{app}"`（跨副本租约）。

**过程中发现并修复的环境/产品问题**（各有独立证据链）：
1. remote-k8s Dockerfile 漏 chmod +x（runtime 镜像入口脚本 644 → CrashLoop）——092c05dc 收编
2. HTTPRoute 全量路由 8086，pingora 代理族（8088）不可达 → userapp content 全程 404/503——71ba1791+e9406189
3. 冻结快照内 __pycache__ 污染（Q05 新校验当场捕获）——71ba1791（禁字节码，未放宽校验）
4. **产品缺口**：operation_guard K8s 分支忽略 wait——无 url start（HTTP 面走 deploy_controlled 本就快失败）之外，内部唤醒/回收路径在 K8s 模式从未真正排队——00913288（+单测 kubernetes_waiting_acquire_polls_until_lease_released）
5. 语义澄清：HTTP 无 url start 走 deploy_controlled 的 try 锁（部署批次既定外部快失败）；"排队等待"契约属于内部路径（唤醒/回收/恢复）——E2E 断言已对齐（8a40656b），spec 表述需后续同步
6. env 收敛以 Deployment resourceVersion 判并发，控制器 status 写入会误触发 "changed concurrently" 并落 RecoveryRequired（不可重试）——本轮以静默等待规避；**产品改进项**（应比对 uid+generation+spec）
7. 卡锁恢复实操 ×4：操作 RecoveryRequired + 租约保留时，retry 仅认最终证据（step=deployment_completed），runtime_created 级失败需操作员删租约 ConfigMap + 手工回收——恢复手册已入 memory

## 三、组件测试（本轮修复对应）

- file-server 315/315（F01/Q01/Q02/F04 回归 8 个）
- app_manager 182/182（带 RCODER_RUNTIME_IMAGE_DIGEST；无 env 180/181——storage_expansion 环境门控既有，前轮已归因）
- docker_manager 全 features 204/204、默认 85/85（K01/K03 回归）
- rcoder 默认 322/322（K02 上层 worker 回归：写后 Conflict→RecoveryRequired+租约保留+后续围栏；事前拒绝对照 Failed+释放）
- app-cli 176/176（R 系列 9 个回归：Start→Restart 身份序列、NotActive 双分支、Settled 取消、终态单调、kernel 不可用门控）
- 工具单测 36/36（chat 结构化解析、失败路径双错误、Make 参数实验、T01 传递复验）
- cargo fmt --all --check 通过；clippy 各 crate 零警告

## 四、本地 Compose 回归

`make dev-hot`（release 2m12s 重编 rcoder）→ 三组套件：

- **test-e2e（userapp 组）**：39/41 pass。2 失败同根因：**环境门控前置缺失 `E2E_SQLITE_BINARY_SHA256`**（冻结镜像构建产物，dev-hot 形态无此产物；sqlite_compose_recreation 在前置即失败，docker_lifecycle_crash 因其 KeyError 连带）——即前几轮 docker_lifecycle_crash 待归因项的根因；门控失败先于任何行为断言，与本轮改动无关，非回归。完整通过该组需按冻结镜像工作流提供该产物（待办）。
- **test-e2e-compose（chat/SSE 组）**：57/60 pass（SSE 20、userapp 族 28、webchat 4 全绿）。3 个 cpp 预览用例失败，干净窗口单独重跑判定：cpp_lifecycle_envelopes 8/8 业务断言全过、cpp_preview_http 5/5 业务断言全过（含 /proxy/{port}/ 预览 200 与停止后 502），仅共有的 metrics_diff 基线探针断言失败（环境断言，非业务逻辑）；cpp_hmr_websocket 卡死于 WS 升级（pingora 8089 收到 Upgrade 请求后无任何响应，curl 复现；本轮未触碰 rcoder-proxy——既有环境问题，留专项）。附带发现：该用例 connect_async 无超时包裹，会无限挂起阻塞整组（测试健壮性缺口）。另一处失误记录：compose 组运行期间本人追加了本验证文档，触发启动器 source-changed 终检报错（Error 2）——启动器行为正确，57 个已完成用例结果有效；cpp 三例已在其后无源码变更窗口重跑
- **test-e2e-compose-deploy（制品部署组）**：1/1 pass（compose_userapp_deploy::userapp_deploy_full_chain；userapp 组内同用例亦 pass）

## 五、分开声明的验收边界

- 实现完成：第四轮 Q01-Q06、K01-K03、R01-R06、F04 + 本轮 E2E 发现的 K8s 等待语义缺口
- 组件测试通过：见上；环境门控用例（storage_expansion、sqlite/docker-crash 组）如实记为未运行/受阻，不视为通过
- K8s 验收：smoke/gateway/userapp 全绿、chat 2/3+retest×2（LLM 抖动归因）
- Compose 验收：userapp 组 39/41（2 个环境门控）；chat/SSE、deploy 组见上
- Java 接入：本轮未在 131 部署 Java 侧，透传链维持前轮只读核验结论，接入待验保留
- 未完成/待办：F02/F03/F05、Q07、R07/R08、K04/K05；env 收敛误判（resourceVersion）改进；部署确认预算改进方案（specs/userapp-deploy-budget-and-recovery/plan.md，用户待批）
