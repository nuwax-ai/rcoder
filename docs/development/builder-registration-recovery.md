# Builder 完成回执与注册恢复

空闲回收后重建 builder 时，普通 EnsureBuilder 的完成回执可能没有
`resource_binding`。把这种有效完成确认一律排除，会使保留旧工作负载身份的
RCoder 副本持续返回 `replacement receipt is unavailable`。

登记恢复以同一应用、生命周期内已经成功的 EnsureBuilder 操作和对应运行时
回执为依据。它验证完整执行上下文、已收束的创建租约、当前 StatefulSet 与
Pod 身份及工作区卷，并在控制面的根 CAS 和停止/删除访问围栏下登记。
多个操作可以确认同一资源；已有绑定保留原操作身份，不改成最新请求。
已有 predecessor/PVC 约束即使其回执已被清理，也仍须通过验证。

这是登记确认，不证明该操作最初创建了工作负载，也不授权新的容器变更。
旧回执省略的冗余 workload UID，只能从已经验证的回执 target 补全；非空
矛盾值拒绝。注册表使用稳定的 StatefulSet 逻辑名称，Pod 名及 UID 单独核验。

同库 PostgreSQL 模式下，普通创建完成也同步提交绑定及注册，再发布本地
镜像，避免异步写入被拒绝后仍返回内存成功。存量注册的旧 Pod/工作负载没有
绑定时，必须有同一当前生命周期的精确成功历史证明；不能凭名字或端口接管。
同一个物理 Pod 缺失 workload UID 时保持原注册代次；新 Pod 则采用新代次，
只退休旧 Pod 的登记。地址更新同时刷新共享的容器视图。

StatefulSet 原生重建 Pod 尚未产生新完成确认时，读取路径返回受理的 Ensure
确认流程，不发布旧地址，也不以发现结果直接替代登记授权。停止、删除、
跨生命周期或身份冲突仍按原保护拒绝。

## 数据源范围

新增普通同步登记只用于能够确定共用 SQL 连接命名空间的 Agent/UserApp
PostgreSQL 配置。独立 UserApp PG 配置的普通登记保持原行为，不要求把
生命周期记录复制到 Agent 库。独立数据库间的工作负载替换登记仍有既有
事务边界限制，本次没有增加跨库事务或静默降低替换校验。

## 回归入口

PostgreSQL 测试必须使用一次性测试库，设置 `RCODER_PG_TEST_DSN`，并设置
`RCODER_PG_TEST_STRICT=1`，防止缺数据库时跳过。焦点测试：

```bash
cargo nextest run --locked -p shared_types -p rcoder-storage -p rcoder-engine \
  -p docker_manager --all-features --no-fail-fast \
  -E 'test(builder_control) or test(builder_replacement) or test(registration_tests) or test(builder_creation_receipt) or test(same_physical_container) or test(connection_scope)'
```

完整受理、HTTP 就绪、完成登记和另一个 PgStore 重载的集成反例需要显式执行：

```bash
cargo nextest run --locked -p rcoder-engine --all-features --no-fail-fast \
  --run-ignored only \
  -E 'test(pg_same_statefulset_new_pod_without_completion)'
```

该用例使用动态本地端口和受控运行时，检查实际数据库及请求事件，同时断言
没有创建、停止或删除物理容器。它不替代真实 Kubernetes 镜像部署验收。
