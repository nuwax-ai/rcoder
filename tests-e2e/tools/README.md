# 严格 userApp 回归入口

`make test-e2e` 运行基础、dev、build-rules、Docker 热部署故障与完整制品部署链。
`make test-e2e-compose` 保留共享 chat/SSE 场景并补齐 build-rules。

选择套件：`make test-e2e E2E_SUITE=compose_userapp_faults`。
选择场景：`make test-e2e E2E_FILTER=userapp_hot_deployment_builtin_contract`。
选择未命中、必要环境缺失、skip、aborted、缺报告、硬断言失败均返回非零。
基础设施用例不要求 LLM key；真实 chat 场景仍需有效模型配置。

每次调用使用唯一 run ID，报告固定写入 `tests-e2e/reports/<run-id>/`。
`manifest.json` 保存计划，`summary.json` 聚合实际结果；每个 libtest 场景独立进程与目录。
源码指纹覆盖 tracked 和未忽略的 untracked 文件内容；镜像记录不包含容器环境变量。
独立执行 `cargo test --workspace` 仍采用环境门控，不代表严格 E2E 验收通过。

`hot_contract.py` 使用真实本地 Docker 镜像 `dev-app-runtime:latest`，启动本次 run 标签限定的容器。
A/B 制品由本机 HTTP 服务器提供；受控阻塞制造并发窗口，断言操作 ID、manifest ID、实际响应、容器 ID。
冷启动通过 env 下发 A 的独立操作与代次；热部署 B 后重启同一容器，必须保留 B 的操作、制品与真实响应，不能被不可变的 A 启动 env 覆盖。成功断言同时要求协议 4、匹配的操作/请求/制品/代次及已持久化的部署成功状态。
准备失败保持 A；切换后失败保持失败，测试通过显式重新部署 A 完成手动回滚，不要求自动恢复旧版本。
故障覆盖 404、截断、SHA、坏 ZIP、缺 lock、读取空闲超时及不安全链接。
按用户要求，app-cli 不再设置下载／解压容量及条目限额；B 制品超过测试中保留的旧 APP_DEPLOY_MAX_* 配置，必须成功部署。路径和符号链接安全检查仍保留。
它不调用或替代 AI，不能作为真实 LLM 测试证据。

运行前必须先构建 builder/runtime，并确认 Compose 的 `RCODER_RUNTIME_IMAGE_DIGEST=dev-app-runtime:latest`。
生产集群与远程开发集群不属于默认入口；K8s API 契约测试使用本机适配器。
修复编号与行为不变量见 `specs/userapp-review-repair/spec.md`，实际验证证据见该目录 `tasks.md`。

`contracts.py` 固定热部署与完整链的必需断言名：早退后写 pass 也会因缺步骤失败。
取消/超时会结束本次测试进程组，尚未完成的计划场景记为 aborted。
清理记录位于各 case 的 `resources/`；只处理随机 case 名称空间及创建响应登记的不可变容器 ID。
已有容器不因名称前缀相似而被删除。清理失败和诊断采集失败独立计入结果。

完整部署前会探测实际镜像的 Python SOABI、架构和 Java 版本，拒绝旧 builder 基础镜像与 runtime 配对。构建可通过 `AGENT_BASE_IMAGE` 指定本地已验证的基础镜像；也可先运行 `make docker-build-agent-base` 重建默认基础镜像。用 `python3 docker/verify-userapp-toolchains.py --builder <image> --runtime <image>` 单独检查，输出包含不可变镜像 ID。
