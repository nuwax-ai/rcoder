# 使用远端 K8s 开发测试

安装 Mutagen **0.18.1**，Linux 准备 Docker Buildx、Python 3、kubectl 和可用 kubeconfig。Mutagen 通过 SSH 自动安装对应的同步 agent。

将 `tools/remote_k8s/env.example` 中的配置追加到仓库 `.env.local`。SSH 推荐使用 `~/.ssh/config` 别名；不配置密码。镜像仓库必须允许 Linux 推送，并允许全部 K8s 节点拉取。HTTP registry 需要节点预先配置，工具不会修改节点 containerd 或全局 Docker 配置。私有输出镜像可配置 `REMOTE_K8S_REGISTRY_AUTH=docker`：只从远端 Docker 配置提取目标 registry 的 inline auth，注入专属 namespace 的 Secret；不复制其他 registry 凭据。

```sh
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-sync-status
make remote-k8s-verify SUITE=smoke
make remote-k8s-test SUITE=gateway
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=chat
# 每次需要测试新源码时，重新构建、部署并运行：
make remote-k8s-verify SUITE=all
make remote-k8s-logs
# 停止服务但保留数据：
make remote-k8s-down
# 单独停止实时同步：
make remote-k8s-sync-stop
```

`build` / `deploy` / `test` 也可分步执行；`test` 验证当前已部署镜像，并分别记录服务端快照和本地测试源码摘要；验证最新服务端修改请使用 `verify`。`userapp/chat/all` 测试执行中的本地源码漂移仍会失败；`smoke/gateway` 运行已加载的只读探针，只记录本地漂移，仍严格检查部署身份。修改同步忽略规则后先 `sync-stop` 再 `sync-start`。并行命令会立即报告锁冲突。Gateway 端口自动分配，实际 URL 在 `.remote-k8s/<环境ID>/deployment.json`。

默认配置：`rcoder-e2e-soddy`、4 核/16GiB、4 个编译任务、开发 profile（无调试符号）。三种基础镜像必须包含项目正常运行所需的 CLI、浏览器、语言工具、Pingap 等运行时，采用已验证发布镜像；它们不是裸 Linux 镜像。项目二进制每次从本地快照编译。升级这些运行时脚本/工具时应同步检查基础镜像契约。

同一私有 registry 支持跨仓库层挂载时，可设置 `REMOTE_K8S_MOUNT_BASE_LAYERS=true`，直接复用基础镜像已有层，减少首次上传。此选项要求 HTTPS、inline Docker auth 和服务端挂载支持；不支持时明确失败，可关闭后使用普通推送。它不改动任何已有镜像标签。

长任务日志在执行时写入 `.remote-k8s/<环境ID>/builds/` 和测试报告目录，可直接 tail。Mac E2E 使用专属 `e2e-target` 缓存，避免与其他 Cargo 任务争锁。

测试报告保存在 `.remote-k8s/<环境ID>/tests/`，原 E2E 详细报告仍保存在 `tests-e2e/reports/`。日志可能包含业务内容，仅保存在本地忽略目录，不自动上传。

`down` 保留 namespace、Gateway、Secret 和所有 PVC。它不承担数据销毁或旧快照回收；需要回收磁盘时，先停止同步与构建，按 receipt 人工确认本环境快照/BuildKit 缓存后处理。不要删除共享 CephFS 根数据。

## 验证记录（2026-09-13）

- 新工作流 11 项、远端清理后端 2 项、原启动器 15 项、UserApp 启动器 4 项：共 32 项回归通过。旧 Make 测试入口 dry-run 保持原命令。
- Mutagen 0.18.1：新增/修改/重命名/删除/忽略及 SSH 中断恢复已实测；快照测试覆盖内容、可执行位、链接、残留隔离和编辑竞争。环境锁拒绝并发命令；真实 SSH 断连会终止本次远端构建。
- 三种 amd64 镜像实际构建、推送成功，按 digest 部署。最新已验证构建为 `20260913T134702Z-3ef9c088`，源码摘要前缀 `ea9b25c5bb2a`。完整清单、基础镜像和输出 digest 见 `.remote-k8s/<环境ID>/build.json` 及 `builds/`。
- 私有仓库实际复用 RCoder 68、computer 155、runtime 38 个基础层；computer 首次复用后的上传层耗时约 7 秒。这是本轮观测值，不是不同 CPU 的性能对比。
- PostgreSQL、两个 RCoder 副本与 API smoke 已通过；动态 agent Pod 真实运行，清理删除本 case 的 STS/普通 Service/headless Service，保留 PVC。
- `down` 已验证停机前后全部 PVC 名称/UID 相同，部署可恢复。工作负载、Service、PVC、HTTPRoute 的跨 namespace 基线检查通过，没有修改其他测试服务的配置。
- 本轮为跑通当前工作树构建，顺带修正几处编译阻断：缺少 sha2 依赖、sha2 0.11 摘要格式化、冗余 Rust 路径限定，以及 tracing 注解引用已改名参数。没有回退工作树中其他开发修改。

### 尚未通过的真实验收

1. **Gateway**：新 Gateway 的 Accepted/Programmed 正常，但实际 NodePort/ClusterIP 请求超时；既有 Gateway 的 NodePort 也复现超时。Envoy 已收到监听器和后端，连接失败；全局网络根因尚未确定，未修改 Cilium、k3s、iptables 或既有 Gateway。
2. **UserApp**：已修正测试部署缺少 builder 镜像项造成的 ImagePullBackOff，当前 builder 可 Ready。并发 workspace 请求仍返回 `ERR_CONTAINER_ERROR`，操作进入 `RecoveryRequired: Builder runtime ownership conflict`。观察到 STS owner 注解存在、Pod 标签只有应用 identifier；运行时 owner 回读链需要应用层继续修复。完整构建/热更新/生产路由验收未通过，未放宽断言。
3. **Chat**：三个真实 LLM 场景（入口轮换、断线续传、新会话跨入口）均通过；但另一路开发在测试过程中改动了本地源码，严格启动器将整轮判为失败。不能将场景通过等同于整轮通过。
4. **完整 verify/all** 尚未通过。早期 verify/smoke 因本地漂移退出；smoke 已改为只读探针规则，并已单独重跑通过。后续 userapp/chat 完整验收需要在测试期间保持工作树稳定。

失败报告、RCoder 文件日志、agent stdout、Pod/事件证据位于 `.remote-k8s/<环境ID>/tests/`；`gateway-diagnostic.json` 记录网络对比，`down.json` 记录 PVC 保留结果，原 E2E 明细位于 `tests-e2e/reports/`。

两个失败的 UserApp (`e2e-k8s-0b4fd6b392b341eb`、`e2e-k8s-fe51710e219d4cf5`) 的清理 API 返回操作冲突。其计算工作负载已通过 `down` 停止，数据卷和恢复证据保留，未强制删除存储或修改数据库来消除失败。
