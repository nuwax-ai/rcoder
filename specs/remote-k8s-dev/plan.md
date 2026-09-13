# RCoder 远端 K8s 开发测试实施计划

## 同步与快照

固定 Mutagen 0.18.1 的 `one-way-replica` 模式，会话标识包含本地仓库路径、SSH 目标和远端目录。读取根 `.gitignore` 与强制排除规则；规则变化要求重新建立本项目会话。环境配置只通过 Python 解析 dotenv，不将其作为 shell 执行。

构建顺序：本地清单 → Mutagen flush/错误检查 → 远端独立复制 → 路径/类型/SHA256/可执行位/符号链接校验 → 再次核对本地清单。只复制清单中的文件，远端额外文件不进入快照；变更竞争最多重试三次，持续编辑则失败。Git 已跟踪但被忽略的生成文件也不进入快照。

## 远端构建

专属 docker-container BuildKit builder，默认 CPU 4 核、内存 16GiB、Cargo jobs=4。持久化 registry/git/target cache，cache mount 采用 locked；开发构建关闭调试符号。

运行时覆盖层使用 `COPY --link`，避免为了覆盖少量二进制而读取完整基础文件系统（[Docker 官方说明](https://docs.docker.com/reference/dockerfile/#copy---link)）；目标路径必须是普通目录。自包含 Dockerfile 编译 RCoder、agent_runner、app-cli、file-server-proxy，覆盖到显式配置的完整 RCoder/computer/runtime 基础镜像中。基础镜像先解析 digest，输出三种带 namespace 前缀的唯一标签镜像并记录 digest；运行时浏览器和语言工具链无需每次安装。使用 BuildKit 内置 Dockerfile frontend，支持独立配置 Docker Hub 和 Debian 软件包镜像源，不修改全局 Docker 配置。可选启用同 registry 的基础层挂载，在仓库端复用已存在的 blob；HTTPS/inline auth/挂载能力不满足则明确失败，不覆盖已有标签。

## 部署与互斥

环境命令持有 Linux flock。远端构建另有执行锁，并绑定客户端心跳、环境所有权 token 与进程组；取消、断连或心跳超时会终止本次构建，执行锁在清理结束前不释放。资源带 `rcoder.dev/environment` 标记，对已有名称先验证归属，拒绝接管。所有 kubectl 命令显式指定 context 与 namespace。

Python 渲染 Kubernetes JSON，无外部 Helm 仓库或相邻 checkout 依赖。资源包括专属 PostgreSQL、两个 RCoder 副本、Role、只读 PV ClusterRole、共享工作区 PVC、Retain CephFS 根 PV/PVC。ConfigMap 按内容命名且不可变，镜像按 digest 部署。私有镜像可显式选择导入远端 Docker 中仅目标 registry 的 inline auth，Secret 只存在测试 namespace，供 RCoder 与动态 Pod 拉取。

RCoder API 使用配置的独立 NodePort；专属 CiliumGatewayClassConfig 设置 NodePort，由 Cilium 为专属 Gateway 服务分配另一端口。读取并验证 ownerReference 后记录实际 Gateway URL，不 patch 控制器生成的 Service。

`down` 先停止 RCoder 控制器，再缩容测试 namespace 的动态工作负载；不删除 namespace、PVC 或 Gateway。

## E2E 与报告

`smoke` 验证两个副本、PVC 和 API；`gateway` 验证 Gateway/HTTPRoute 条件和实际 HTTP；`userapp` 复用 Python 真 K8s 验收，验证跨副本文件、真实构建/部署/热更新和代理访问；`chat` 复用严格 Rust `k8s_lb` 启动器，真实调用 LLM；`all` 串行执行全部。

UserApp 原 namespace 默认不变，新增 `rcoder-e2e-*` 必须提供匹配的环境归属。聊天启动器新增显式 `--remote-k8s` 选项，使用 namespace 归属和 case ID 清理本次 STS/Service，不检查或清理本地 Docker。Rust 清理支持 `TEST_K8S_CONTEXT`；新入口始终传入 context，既有未设置时行为不变。

本地 `.remote-k8s/` 保存源码、构建、部署和测试凭据（receipt，不含登录密钥）。服务端快照与本地测试源码分别记录摘要；userapp/chat/all 运行中源码变化失败，smoke/gateway 只读探针记录漂移但不因此失败；前后检查 Deployment UID/generation/digest/Ready 副本，并记录实际 Pod imageID/containerID；对其他 namespace 既有工作负载、Service、PVC、HTTPRoute 的 UID/spec 摘要做前后比对。失败采集 RCoder 文件日志、agent stdout、Pod 状态与事件。
