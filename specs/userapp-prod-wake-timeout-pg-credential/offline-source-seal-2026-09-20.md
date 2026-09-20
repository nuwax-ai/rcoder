# Stopped 容器凭据切换：离线源代次交接

## 问题与语义

普通管理启动仍会启动容器入口及业务，不能用于“先交接、再换代”的凭据更新。物理容器已停止时使用一次性 helper；管理进程仍在运行时继续使用原 source-seal API。不能用检测到端口关闭替代原资源停止和 UID 验证。

## 协议

1. 按原 operation、lifecycle、scope、物理资源 UID 和 generation 校验，持久记录 offline 请求。
2. 原镜像、原 workspace/状态卷建立确定性 helper；只覆盖入口执行 `app-cli seal-source --workspace <原路径>`。stdin 为 RuntimeGenerationHandoff JSON；stdout 为单个 RuntimeGenerationSourceSeal JSON；非零表示未确认成功。
3. helper 的实际 UID/spec 落盘后才执行。Docker create 与 start 分开；K8s 创建带 scheduling gate 的 Pod，再按原 UID/resourceVersion 解 gate。
4. 记录合法 seal 后确认 helper 已退出，再按 UID 清理 helper，不删卷，然后进入既有换代和 prepared/PG 凭据应用流程。
5. 断连、超时、返回丢失，保留原身份与 helper；重试查询原 helper，不另建第二个，也不伪造成功/释放租约。

## app-cli 边界

- 校验原 PROJECT_ID、APP_DEPLOY_GENERATION_ID、持久 identity、原 workspace 路径。
- 获取同一 OwnerGuard 和 journal 锁，校验旧进程清理证据与内核未知操作；不启动 listener、PG、业务或 supervisord。
- 校验已确认制品、迁移记录、desired revision，复用运行中 source-seal 的原子封存逻辑。
- 旧目录迁移同时移动 journal、coordinator 与 source-seal，不能留下第二份权威。
- 原身份与 desired 保持不变；同授权重放返回同 seal，冲突授权拒绝。
- `run` 与 `serve` 都必须遵守源代次封存，不能借环境变量更名绕过原 journal。

## 验证状态

离线入口和旧 run 入口此前聚焦 2/2 通过；source-seal 随旧 journal 迁移的追加聚焦 15/15 已通过。Docker/K8s 实现全 features/all-targets check 已通过，尚无真实 Stopped 凭据更新通过证据。不得把本设计文档作为部署验收结果。
