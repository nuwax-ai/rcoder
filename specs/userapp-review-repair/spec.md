# userApp 定向修复规范

基线：feature-userapp / d886b67。正式 userApp JSON 入口使用 HTTP 200 + HttpResult；旧 TS 兼容入口保持原契约。

|编号|必须保持的行为|
|---|---|
|R01|失败创建只能补偿本操作创建的资源，不得删除竞争赢家或 agent PVC|
|R02|部署准备失败不停止旧服务；恢复代码不声称回滚数据库迁移|
|R03|部署原子受理，操作、请求 release、制品 release 身份分离，终态不能串代|
|R04|业务 env 不覆盖平台参数；hot + 实际 env 变化在副作用前拒绝|
|R05|本副本注册表 miss 不等于容器不存在；查询错误不得伪装不存在|
|R06|K8s 条件更新在实际写入执行，失败者不覆写运行配置|
|R07|SSE 订阅之后产生的终态必须送达；越过快照终态的游标立即结束|
|R08|stop/cancel 使旧启动代次失效，返回后旧任务不得重新启动|
|R09|失败及取消清理本操作下载和 staging，不删除活跃操作临时文件|
|R10|下载和解压有资源预算及读取空闲超时，实际写入计数|
|R11|正式入口错误信封、提取器、OpenAPI 和英文消息一致，兼容入口不变|

测试必须固定行为而非迁就实现。组件竞争测试与 Docker 黑盒验收分别报告；K8s 本轮仅编译和 API 契约测试，实机测试等用户部署后执行。

## 固定回归映射与验收层级

|问题|组件／协议回归|Docker 黑盒锚点|
|---|---|---|
|R01|`create_app_runtime_failure_does_not_delete_unowned_resources`；K8s `concurrent_creates_never_compensate_the_winning_deployment`|运行资源登记与按 ID 清理；不把单实例 Docker 用例称作 K8s 并发证明|
|R02|app-cli prepare 保留旧 code、恢复终态测试|双引擎故障套件：A 健康、失败 B 恢复、旧迁移不重跑|
|R03|`concurrent_deploy_admission_has_one_winner`；旧失败终态不被恢复 Running 覆盖|slow B 期间竞争 A 返回 409；终态核对 operation/request/manifest 身份|
|R04|`hot_env_change_rejected_before_runtime_mutation_equal_env_removed`|完整 start(url) 链；配置与进程身份留痕|
|R05|`replica_without_registry_resolves_runtime_and_preserves_query_failure`；`healthy_wrong_family_and_higher_priority_identity_are_rejected`|开发停止、取消及无启动查询；跨副本实机一致性保留为后续层级|
|R06|真实 kube 客户端本机 API 适配器：同版本仅一个写入成功、UID 补偿、丢响应不删可能活跃配置|Docker 不提供 resourceVersion CAS，不冒充 K8s CAS 验收|
|R07|`stream_close_tests` 五项：订阅/回放期间终态、越界游标、lagged、重连|完整构建链终态 SSE 回放|
|R08|`stopped_generation_cannot_commit_after_preparation` 与 `stale_preparation_neither_promotes_nor_leaks` 固定启动及目录提交窗口|dev stop/restart/cancel 生命周期场景|
|R09|`preparation_lease_protects_active_staging_and_reclaims_stale_owned_files`|双引擎每个失败操作均检查 incoming/staging 无残片|
|R10|`zip_limits_count_actual_extracted_bytes_and_entries`|下载、单文件、总量、条目与 idle 小预算触发|
|R11|正式路由提取器与 OpenAPI 守卫；保留专用错误码|正式入口 HTTP 200＋错误码；旧 TS header、已删除路由及字节流原协议|

`tests-e2e/tools/contracts.py` 是热部署和完整链的必需步骤清单。修改实现不自动修改此清单；缺少步骤即使记录 pass 也失败。每次严格入口冻结测试二进制并记录 SHA256，源码在编译或执行期间变化即判为非冻结验收。
