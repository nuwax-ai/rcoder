# userApp 定向修复实施方案

1. 测试设施先建立严格入口、唯一 run ID、全进程汇总、必测场景与报告完整性检查。基础设施门控不依赖 LLM；显式 E2E 缺失前置必须失败。
2. runtime 补偿使用创建身份凭据；K8s 更新使用版本化配置与 Deployment 条件提交；注册表未知使用按服务族隔离的只读查询。
3. app-cli 部署拆分准备/激活/恢复，原子受理并追踪独立操作身份；平台 env 单独保留，hot env 变更前置拒绝。
4. 开发任务启动提交与停止共享代次保护；SSE 关闭依据订阅快照；临时目录由操作拥有，按用户修订，app-cli 不设下载容量、解压总量、单文件大小及条目数限额，保留默认 60s 读取空闲超时。
5. 正式业务错误出口与提取器统一，旧 TS 兼容路由保留。跨 crate 契约定义在 shared_types。
6. 验证 workspace fmt/Clippy/default+K8s/tests，独立 app-cli gates；构建 builder/runtime，执行 make dev-hot，校验实际运行镜像，再严格 Docker E2E。

不登录真实集群、不发布、不推镜像、不清理已有用户资源。不引入全域分布式事务；回滚不逆向执行数据库迁移。

## 实施细化

- 创建失败补偿收敛在 runtime 内部：Docker 已获得的容器 ID、K8s 本次 POST 返回的 UID 是删除凭据；app_manager 完全移除按 app_id 的补偿删除。上层无需接收凭据再发起清理，避免把误删能力重新带回消费方。K8s 不确定提交保留配置，明确拒绝用后续探活证明资源归属。
- 条件更新和部署状态跨 crate 载体为 shared_types 的 `AppEnvSnapshot`、`AppMutationPrecondition`、`AppDeploymentOperation`、`AppDeploymentRecovery`。Docker 无 ConfigMap/resourceVersion，保留既有开发运行语义；Docker 热部署后的创建 env 种子重启限制不作为 K8s 能力证明。
- 恢复使用独立 Recover 动作，禁止再次执行旧迁移。生产 builtin 编排存在失败服务时不得报告部署 Running；开发 run 模式保留原有部分运行诊断行为。
- E2E 错误协议迁移同时检查 HTTP 200、具体业务码及英文 message；旧 TS 入口和已删除路由保留原状态。空工作区回归用例须区分同步拒绝与带真实项目的异步构建失败，不能为了通过而放松为空工作区受理。
- 本地工具依赖缓存由 `AGENT_TOOLS_CACHE_KEY` 显式刷新；普通源码重建不再用当前时间强制拉取所有工具。运行报告记录实际镜像与二进制身份。
