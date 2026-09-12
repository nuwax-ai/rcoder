# 实现与验收方案

## 资源边界（runtime_review）
跨crate资源身份/前置条件/结果定义shared_types，通过runtime trait传递。删除先取得应用操作锁再读取校验。K8s DELETE实际带UID/resourceVersion；Docker按真实ID、服务类型与归属删除。
purge在计算面删除前捕获PVC身份；所有复用PVC的创建/重建/唤醒先CAS刷新使用claim，原UID/RV条件删除不得影响新claim。Docker共享数据根应用文件锁覆盖创建、删除、数据清理；所有路由/元数据清理也校验操作归属。冲突不继续删除，404必须确认为该身份已不存在。agent PVC不在范围。
修复旧/新端口快照恢复、watch send_replace和K8s查询错误传播。补确定性测试及K8s真实请求适配器，禁止真实集群操作。

## PG（core_review）
独立项目生命周期代次，load/sync/内存快照保留，不复用时间戳/session/定位键。删除携带捕获代次，清会话携带身份集合，容器关联删除携带物理容器与项目身份；SQL条件不匹配为Superseded，绝不无条件重试。墓碑拒绝已删除代次旧快照复活。内存变更/操作登记按项目串行、网络锁外，直写和降级共用操作构造。保留受理降级并显式结果。flush返回明确剩余/错误，并发重复关停共享同次最终结果。
增量迁移与已有数据回填；不承诺旧writer混跑，说明停写/排空/迁移/恢复。真实PG测试只使用本run专属postgres17及数据。

## Agent（agent_review）
命令入队async可取消返回Result，不持DashMap await。满订阅移除并cancel。HTTP SSE明确EOF/取消/终态/心跳及定向清理；保留轮终态清ring。
AgentRegistry单writer处理具体upsert/remove，阻塞worker加文件锁读最新文件合并，独占tmp原子替换后刷新内存；调用方cancel不打断已受理提交，写错误上抛。

## 主审实现
主平台SSE发送/异常兜底受取消且满队列断流；开发任务先检查构建错误与取消再幂等，同队列Done作为保序屏障，移除50ms排空与detach，120s缺Done失败。static管理器保存配置/状态/取消句柄，同端口更新root，换端口先bind再切换，移除回收；失败恢复同步旧静态配置。
新增错误遵循现有信封/SSE协议，公开错误同步OpenAPI/i18n，平台日志英文。

## E2E和构建
新增并发生命周期与pg_storage_faults，独立postgres17 Compose项目，不改日常内存模式。所有严格场景有必需稳定断言目录，未知场景fail closed；保留唯一run/源码指纹/镜像身份/逐进程报告/定向清理。infra不依赖LLM，AI场景真实调用。
先红后绿；workspace fmt/clippy/kubernetes clippy/tests、PG check/集成、app-cli独立fmt/clippy/tests、Python工具门禁。冻结前app-cli升patch，更新lock。
核对跨仓同步差异后依次make docker-build-app-runtime、make dev-restart、make dev-hot；修复dev-restart失败被echo掩盖。显式新builder/runtime镜像，核验容器/二进制身份；严格make test-e2e及make test-e2e-compose，修复后最终完整验收。
验收成功提交完整依赖契约并打app-cli-v* tag，验证六个npm包及安装。无平台v*tag，无镜像推送，真实K8s运行保留未执行。

## 实施复核补充：Q01 的 PVC 在途写入窗口

仅“writer 条件刷新 PVC claim + purge 捕获 UID/resourceVersion”无法排除 writer 已刷新 claim、尚未创建 Deployment 的反向窗口。补充局部每应用 K8s 操作互斥，覆盖 PVC 复用、计算资源提交与删除/存储清理整个边界。锁资源以操作 ID 与 UID 证明归属，竞争立即冲突，不按 TTL 自动偷锁；崩溃遗留锁须先证明原操作已停止才可人工恢复。此为资源生命周期局部互斥，不扩展为全域事务。

## 冻结前归属复核
Docker生产归属采用shared_types的app-id标签常量；builder独立inspect真实service-type/identifier。更新先prepare镜像与配置，再按旧物理ID条件删除；明确PreparationFailed只表示计算变更尚未开始，Docker调用方恢复旧路由并释放操作保护。删除后create/start失败不自动声称恢复成功，保留不确定操作标记。agent PVC ensure允许创建，只有destroy禁止agent族。

## 注册表读取快照
按用户追加建议，AgentRegistry内存视图改为ArcSwap<RegistryEntries>：查询一次load并在同一不可变视图完成遍历；blocking writer仍持串行写锁及OS文件锁，读取最新文件、应用具体操作、原子落盘成功后store完整Arc。持久化提交不做无锁化，失败不发布，取消不打断已受理提交。读者持有旧视图不会阻止发布，也不会看到原视图被原地修改。沿用agent_runner现有arc-swap依赖，无新增依赖。

构建入口采用顺序递归make，先agent-runner的实际工具链检查和构建，再master；保留原30秒探测截止与兼容性断言。用真实Docker合成上下文验证排除规则，用隔离子目标及make -j4验证顺序和失败传播。
