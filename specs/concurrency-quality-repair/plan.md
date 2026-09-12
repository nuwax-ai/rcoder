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
主平台SSE发送/异常兜底受取消且满队列断流；开发任务先检查构建错误与取消再幂等，同队列Done作为保序屏障，移除50ms排空与detach，120s缺Done失败。static管理器保存配置/状态/取消句柄，同端口更新root，换端口先bind再切换，移除回收；切换失败回收静态监听；旧业务恢复由显式重部署完成（D03）。
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

agent-runner 编译 Dockerfile 增加 scratch artifacts 最终阶段，仅 COPY 两个既有路径的二进制，提供默认 CMD 以兼容 docker create；镜像层检查与容器导出检查分离，前者禁止隐藏的源码与缓存，后者核验内容与执行权限。

PG、热部署及构建测试创建前持久化 ownership 和 pending 状态。Docker CLI 超时不等于 daemon 取消；清理只在已知创建结束或捕获唯一归属一致的物理 ID 后确认成功。迟到资源先更新凭据再删除；通用兜底不得绕过专用创建状态检查。保留原失败结果，后续收束另附证据。

真实E2E增补：HTTP purge由持有Arc service的独立任务执行完整service方法，取消caller不取消收尾，worker错误/panic仍沿原保护保留marker。Docker缓存失效按物理ID谓词进行；创建使用无缓存回退的权威inspect，Running无地址保留，停止容器DELETE force=false。原重建路径保留语义，但捕获actor身份一次，后续查询与删除只用捕获ID。清理报告保存脱敏后的传输类别、退出码及业务code/message，成功定义和截止时间不变。

重建仍只在原有project_id存在边界触发，但查找键改为已经派生的PreparedAgentConfig.container_id；用同时登记project/pod两实例的真实preflight→start_prepared契约测试证明不访问错误实例。

- Docker缓存用Arc身份token记录查询代次，所有失效与条件回填共享短提交锁，Docker/网络I/O留在锁外。状态双别名一次条件发布，网络成功及404负缓存同样校验；无关失效仅保守丢弃回填。stop_by_id已按物理ID退役，移除调用方末尾按逻辑键无条件remove。

- common统一无I/O上下文门控，由Compose、PG、K8s与跨进程场景锁受理入口调用。Env::load保留配置读取职责，不将普通单元测试误当E2E；严格入口继续使用固定报告身份目录和必经断言。

## D01–D06：统一部署实施方案（优先于此前恢复描述）
shared_types 定义协议 v4、操作代次和独立 AppDeploymentStage。env 冷启动与 HTTP hot 共用原子受理入口。冷等待只认匹配 operation_id 的部署段成功，不依赖请求或 manifest release_id。非空 SHA 完整校验后统一小写，自动 release_id 不再从 SHA 截断派生。
app-cli 在卷根以应用文件锁、独占临时文件和原子替换保存 generation/operation/目标/有效制品/切换阶段。同代次重启恢复有效版本，新代次覆盖旧恢复意图；切换中断禁止猜测启动。准备失败保留旧业务，切换后失败仅保留现场供显式重部署，不自动恢复旧编排。静态服务管理器与进程组退出确认保留。
hot 成功须 Running、操作匹配与持久化确认，K8s 回写带 resourceVersion 并检查归属；Docker 验证持久化记录而非伪造可变 env。已有 preaccept 回退保留，网络错误不当作不存在；受理不确定保留操作保护。
本批完成后继续 Q01–Q12 冻结验收：独立 app-cli 测试、workspace 默认/K8s/PG gates、镜像构建、dev-restart、dev-hot、严格两套 E2E。所有通过后才提交、app-cli patch tag 与 npm 六包发布。真实 K8s 未执行。

HTTP 等待保持300秒软预算；协调任务独立于请求存活，最长30分钟进行有单次请求超时的对账，超时仍未知则保留操作凭据并报告，禁止当作完成释放。冷部署创建/更新使用借用guard内核，不重入锁，持有到部署段确认与后置SQL结束。hot env在锁内快照重新校验。纯env/secrets保留键校验在获取租约前执行。
app-cli 启动先建立退出证明：supervisord停止业务组；builtin只在新Linux进程命名空间/启动代次证明旧进程已结束后恢复。无法证明的app-cli单进程重启保持pending、要求容器重启；不发布可重试终态。记录放卷根，不进行配额或容量限制。

最终边界补强：hot 完成使用匹配 operation → /ready 业务就绪 → 再匹配 operation 的检查顺序；成功和失败终态均要求 persisted，未确认结果不得释放操作凭据。Existing 启动不得改写未附着或其他 generation 的部署记录；已有工作区的有效制品身份可作为首次 hot 的 baseline，无需虚构下载 URL。协调器进程 scope 独立于部署结果保存：先验证旧进程退出并停止已知业务，再原子保存并读回当前 owner，最后允许编排。

正常停机补强：协调器owner增加Active/Quiescent（旧字段缺失默认Active）；成功启动证明及Active提交后才标记ownership_claimed。关闭受理复用现有锁与409明确拒绝，随后driver返回可检查的退出结果；保留prepare JoinHandle以等待真实停写，关闭静态监听并检查全部结果，最后原子fsync/readback Quiescent。启动被拒、未成功claim、任务panic/error或30秒停写确认超时均保留Active。部署receipt的失败/切换边界不因Quiescent被改判成功。

镜像 ABI 补强：本地 master-base 记录 Dockerfile.base 及 COPY 输入指纹；docker-build-master 在编译前核对，不匹配提示显式重建基础镜像。来源指纹只证明构建定义一致，最终运行层仍执行 rcoder/agent_runner 的无副作用 --version，验证实际动态链接及退出状态。本轮先显式重建 master-base，再按 runtime → dev-restart → dev-hot 重建验收。
