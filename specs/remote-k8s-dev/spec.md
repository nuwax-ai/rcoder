# RCoder 远端 K8s 开发测试规范

## 目标

Mac 保留编辑与测试体验，Mutagen 0.18.1 将源码单向同步到个人 Linux，使用远端 BuildKit 缓存构建 amd64 镜像，在独立 `rcoder-e2e-*` namespace 验证 K8s 运行模式。

## 边界

- 新增 `remote-k8s-*` 入口，既有 Docker Compose、DevSpace 和测试默认目标保持不变。
- 远端源码接收区、不可变构建快照、编译缓存分开；镜像以源码摘要和 digest 关联，未同步成功不得构建旧源码。
- 独立 PostgreSQL、两个 RCoder 副本、namespace 内 RBAC、Gateway 与动态 agent/UserApp 资源。PV 只读发现权限是集群级权限。
- 沿用 CephFS 根聚合访问；独立 namespace 不意味着共享 Ceph 根的安全隔离。
- 不删除 agent PVC、共享存储数据或其他环境服务。`down` 只缩容工作负载，保留持久资源。
- 不引入 Synx、不替换现有镜像发布与 npm 发布流程；基础运行环境来自明确配置的完整运行时镜像。

## 验收

正确同步新增/修改/重命名/删除和忽略凭据，故障或内容不一致立即失败。镜像构建、就绪、真实 API、UserApp 及聊天验收分别报告证据；缺失 LLM 配置、失败、跳过和源码漂移不当成通过。
