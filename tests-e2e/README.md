# RCoder E2E 测试导航

`rcoder-e2e` 覆盖 UserApp 生命周期、构建部署、文件/代理、chat/SSE、持久化和故障恢复。它包含真实平台场景、组件契约和专项故障实验，不能用一类通过代替全部验收。

- [测试逻辑与验收流程](architecture.md)：入口、分组、固定登记、执行、报告和清理。
- [场景运行与前置说明](tools/README.md)：核心聚焦、配对镜像、Python缓存、CephFS、存储和真实agent。
- [远端K8s配置示例](../tools/remote_k8s/env.example)与[Make入口](../make/remote-k8s.mk)：个人环境同步、构建、部署和验收。

## 常用命令

从仓库根执行。保留已有`.env.local`，按配置示例补缺项，不覆盖凭据或无关配置。

```bash
# 查看场景、最近结果；检查三份固定登记。
make test-e2e-list
make test-e2e-check

# 精确运行一条核心链。
CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_idle_recycle_owner_recovery

# UserApp默认组，含需要LLM的真实agent/chat场景。
make test-e2e

# chat/SSE/session/预览及部分UserApp。
make test-e2e-compose

# 七服务模板完整构建部署。
make test-e2e-compose-deploy
```

`E2E_SUITE`覆盖默认套件清单，支持逗号分隔；`E2E_FILTER`按名称子串筛选，不是正则。未命中、缺前置、skip、aborted、缺报告/必测步骤或清理失败均不能通过严格入口。

真实集群和宿主机入口单独选择，目标从配置读取并先确认授权。`make test-e2e-k8s`是旧LB专项，实际运行忽略场景需显式`RUN_LB=1`，不是UserApp部署全链。日常K8s优先使用项目远端工作流。

## 验证边界

- 普通Rust测试使用nextest；严格E2E使用专用Python启动器，逐例执行冻结的libtest二进制。
- 直接`cargo test -p rcoder-e2e`缺上下文时在外部I/O前跳过，不能算E2E通过。
- 工具单测、测试目标编译和登记检查不证明真实场景已执行。
- 测试二进制冻结不等于产品镜像冻结；RCoder/app-cli镜像还须有本轮构建身份。
- 新增用例同步套件成员、报告身份和必测断言，执行`make test-e2e-check`。

## 查结果

默认报告为`tests-e2e/reports/<run-id>/`。先读`summary.json`，再看`<suite>/<case>/process.log`、JSONL断言与`resources/`清理证据。完整布局见[报告与排错](architecture.md#报告与排错)。

`src/common/`提供环境、HTTP/SSE、报告器、资源身份和共享计算流程；`tests/`放Rust场景；`tools/`放启动器与专项工具。完整场景清单以源码、固定登记和`make test-e2e-list`为准，不手工维护第二份清单。
