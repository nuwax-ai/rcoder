# 并发与代码质量修复规范

基线：81b8a480a5f32fa7773ad636f22b8ef969dca5ac。用户已批准 Q01–Q12 修复、独立 PG 集成、Compose 构建验收及 app-cli npm 发布。

| 问题 | 固定不变量 | 必要验收 |
|---|---|---|
| Q01 | 旧删除不得删除新计算/存储身份；冲突不得继续 purge | 删除等待锁竞态、K8s UID/RV请求、PVC新claim、Docker跨进程锁 |
| Q02 | 降级旧删除/会话清理/快照不能影响新代次 | postgres17事务故障与旧操作重放、迁移回填 |
| Q03 | flush成功等于全部落盘；并发重复调用共享结果 | 失败/超时/重复关停 |
| Q04 | 无订阅者时也保留唤醒终态 | 克隆handle后延迟订阅 |
| Q05 | 更新失败不把旧应用路由恢复至新端口 | 真实代理旧端口响应 |
| Q06 | Agent命令不静默丢失；慢订阅不影响其他订阅 | 满1000命令加终态、满订阅取消 |
| Q07 | Agent HTTP SSE在EOF/cancel/terminal退出 | 连接清理、心跳与EOF |
| Q08 | 主平台SSE取消能打断发送等待 | 满队列取消限时退出 |
| Q09 | static配置变化生效、移除及切换失败释放监听；恢复由显式重新部署完成 | 两引擎A/B目录/端口/移除/手动恢复 |
| Q10 | 编译失败/缺Done不成功；服务事件先于终态 | 受控消费延迟、缺Done、运行中编译失败 |
| Q11 | 注册表写入不覆盖新状态，调用方取消不打断受理提交；读者持有一致快照不阻止新提交 | 并发安装/重载/写错/取消/旧快照保留 |
| Q12 | K8s API错误不是不存在 | 实际HTTP错误契约 |

PG故障保持内存受理+异步补写，显式暴露降级。慢SSE队列满立即断开；保留轮结束清ring边界。正式HTTP信封/旧TS入口/真实AI调用保留。不恢复制品容量或条目限制。agent PVC永不删除。不操作真实K8s、既有应用与数据。只定向清理本次run登记资源。

构建验收不变量：Docker构建上下文不包含用户运行目录或本地凭据；聚合构建按agent-runner→master串行执行，即使外层make -j也不重叠，任一步失败停止并返回非零，工具链核验失败不冒充版本不匹配。

构建中转镜像只携带两个可执行产物及父目录，全部镜像层不得携带源码、编译缓存或工具链；原有容器创建及提取路径保持可用。Docker 创建请求超时后，空库存不证明创建已结束；清理必须记录未确定状态，直到本次操作的真实资源身份可核验，不能把迟到资源遗漏计为验收成功。

已受理的HTTP purge在调用方取消后仍完成整个元数据、缓存及操作标记收尾；未知执行结果不得释放标记。物理删除只清除仍指向该物理ID的缓存；创建不能凭缓存复用已删除对象，查询失败不能伪装不存在。创建中的停止容器清理不得强杀并发启动的容器；既有重建流程只删除最初捕获的物理ID，不按逻辑键二次取值。

重建定位复用preflight通过ServiceType::container_identifier派生的标识，不能用project_id覆盖pod_id优先级。

- 缓存失效后，此前在途查询不得回填状态或网络缓存，也不得以晚到404清掉新映射；旧stop结束只能退役捕获的物理身份。

- E2E受理必须先具备run/case/report/testname完整上下文，再进行配置加载、端口锁和外部连接；普通workspace无上下文明确skip，显式strict缺字段必须失败。

## D01–D06：冷、热部署统一契约（本轮新增，覆盖旧自动恢复策略）
- D01 请求 release_id、制品 manifest 身份与 operation_id 分离；冷、热成功必须匹配操作。冷部署段结果独立于业务 readiness。
- D02 每次冷部署建立新 deployment_generation_id，hot 保留代次。同代次重启恢复持久化有效制品，新代次执行新冷部署目标。
- D03 hot 受理前保留既有明确回退；受理后或结果不确定不得冷部署。真实 env 变化提前拒绝。prepare 失败旧服务运行，切换后不自动业务回滚。
- D04 切换前持久化阶段，成功结果发布前持久化；中断切换 fail closed。Docker 不以空操作声称配置持久化成功；K8s 条件写入。
- D05 SHA 完整 ASCII hex 校验，无大小或条目配额；平台部署参数不允许业务覆盖。
- D06 严格 E2E 登记操作、代次、制品、实际内容及容器身份断言；A→hot B→重启仍为 B。真实集群不执行。

### D 系列固定回归映射
| 编号 | 自动化用例/必经断言 | 层级 |
|---|---|---|
| D01 | cold_stage_uses_operation_not_request_or_manifest_identity、healthy_old_service_cannot_hide_failed_deployment、missing_operation_never_falls_back_to_release_id；轻量 URL 精确操作断言 | Rust组件 + Docker E2E |
| D02 | startup_resumes_hot_receipt_identity_and_published_artifact、durable_hot_receipt_overrides_same_generation_stale_seed；hot B 容器重启断言 | app-cli组件 + Docker E2E |
| D03 | hot lease内业务env对比、accepted_hot_coordinator_survives_cancelled_caller、response_timeout_does_not_cancel_owned_coordinator；准备故障/显式恢复 | Rust + 真实HTTP + Docker |
| D04 | journal中断/损坏/写入失败、startup进程退出barrier；hot_env_commit_sends_resource_version_and_preserves_conflict | app-cli + K8s API契约 |
| D05 | sha_validation_rejects_non_hex_and_unicode_without_slicing、invalid_digest_and_reserved_identity_have_no_runtime_side_effects、update_reserved_secrets_rejects_before_mutation_and_releases_ownership | Rust组件 |
| D06 | acceptance_steps 必经步骤遗漏门禁、hot身份变异测试、轻量部署身份/性能独立断言 | Python工具 + Docker |

- D02/D04 补充不变量：业务 readiness 与部署操作结果分别验证；任何未持久化终态均不构成可释放凭据的完成证据。未附着的 Existing 启动不能把旧代部署误判为成功，首次热部署准备失败后连续重启仍保留已确认旧制品及失败操作身份。协调器进程退出证明独立于是否存在部署操作。

- D04 正常停机：本地serve正常退出并确认所有业务、静态监听、准备任务停止后，可在同workspace正常再次启动；只有明确持久化Quiescent才能复用此证明。异常退出、停写未确认、超时、panic或启动归属未取得时保持Active，不得通过正常退出一个被拒绝启动的进程绕过原保护。

- 镜像验收补充：主服务健康不能替代镜像内 Agent 可执行性。最终主镜像中的 rcoder 与 agent_runner 都须在实际运行层执行版本探针并成功退出；基础镜像来源不匹配时在编译前明确拒绝，不能静默复用旧发行版。
