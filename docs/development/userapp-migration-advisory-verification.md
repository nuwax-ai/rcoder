# UserApp 应用迁移失败的隔离 Docker 验收

`tests-e2e/tools/userapp_migration_advisory.py` 使用独立容器和工作区卷，运行真实 app-cli、file-server-proxy、PostgreSQL、任务接口及业务 HTTP。它不使用 AI，不接触已有部署，也不执行构建或发布。验收后只删除本轮创建且标签匹配的容器，工作区卷保留在报告中，便于检查持久数据。

四个场景共用同一容器、卷、sentinel 和管理 owner：

| 场景 | 必须观察到的行为 |
|---|---|
| 非零退出 | 迁移 exit 1；完整 stdout 和 24 KiB stderr 经原 operation 进入任务 SSE；任务 Completed、原操作 Succeeded、实际业务 HTTP 成功 |
| spawn 失败 | 真实不存在的迁移可执行文件；guardian 失败详情可见；清理后同一服务启动，任务和原操作成功 |
| 超时 | 真实迁移及忽略 TERM 的后代持续运行；等待产品固定的 300 秒后清理原进程树，再继续启动；任务成功且日志明确为 `300000ms`，不注入短 timeout |
| 父 Stop | 迁移仍活着、原操作未终态、完整输出已经到任务流时提交 Stop；原操作和任务取消、原进程树退出、业务启动计数不增加、管理 owner 保留 |

每轮都检查容器 ID、独立卷挂载和 sentinel。报告绑定源码摘要、镜像 ID、架构及二进制 SHA256。内部 PostgreSQL 测试密码使用临时 0600 env 文件，报告与 HTTP/SSE 不包含明文；脚本不会导出私有操作请求文件。

先在独立的 Linux 构建目录生成与当前源码对应的两个二进制，并按已有构建流程生成、注册来源回执。脚本使用的回执格式与 `userapp_root_logs.py` 相同：

```bash
python3 tests-e2e/tools/userapp_root_logs.py \
  --source-dir "$PWD" --write-build-source "$BUILD_SOURCE"

# 按仓库既有 Linux 构建流程构建，保持源码稳定；构建目录使用外置工作区。

python3 tests-e2e/tools/userapp_root_logs.py \
  --source-dir "$PWD" --build-source "$BUILD_SOURCE" \
  --app-cli "$APP_CLI_LINUX_BIN" --file-server-proxy "$FILE_SERVER_PROXY_LINUX_BIN" \
  --register-binaries

python3 tests-e2e/tools/userapp_migration_advisory.py \
  --source-dir "$PWD" --image "$FIXTURE_IMAGE" \
  --app-cli "$APP_CLI_LINUX_BIN" --file-server-proxy "$FILE_SERVER_PROXY_LINUX_BIN" \
  --build-source "$BUILD_SOURCE" --report "$MIGRATION_REPORT"
```

`BUILD_SOURCE`、二进制路径、镜像和报告路径由本轮环境提供。所选 Docker context 必须指向本地 Unix socket 或 loopback TCP；工具明确拒绝远端和无法确认的 endpoint。完整运行至少需要真实等待一次 300 秒迁移超时，不能用组件测试或部分场景代替。

仓库的统一入口会为本轮创建独立、只读的验收二进制目录，并登记源码与产物摘要：

```bash
make test-e2e-userapp-migration-advisory \
  USERAPP_MIGRATION_IMAGE="$FIXTURE_IMAGE" \
  USERAPP_MIGRATION_VERIFY_DIR="$VERIFY_DIR"
```

`VERIFY_DIR` 必须为不存在的新目录；构建的 Cargo target/cache 路径仍按项目的外置构建工作区配置。入口内的组件构建与 Docker 验收必须串行，验收期间不更换二进制或源码。

工具契约检查可以独立运行：

```bash
python3 -m unittest discover -s tests-e2e/tools -p test_userapp_migration_advisory.py -v
```

这些检查实际执行 Python fixture、HTTP 和 marker 写入，并校验 manifest、原操作字段、报告裁剪及 0600 文件创建；它们不执行 Docker，不能作为容器验收通过证据。

如果 file-server 先结束任务、原 app-cli 操作仍 Accepted，须按真实失败处理，保留原操作 ID。任务启动的父 deadline 必须覆盖该轮配置允许的迁移、编排及事件排空阶段；不能通过缩短产品 300 秒、放宽任务终态断言或重派新请求使场景通过。

## 2026-10-07 集中修复验收

本轮 app-cli 源码为0.3.16，Node保持22，Pingap保持0.14.3；模板CLI0.1.24已发布。源码输入SHA-256为 `1c58b3775327849974877d825f508aadc77b3c5286d1a93aa4dba3610fe9a910`。独立app-cli396项、根默认3190项及全features3456项组件通过，fmt/Clippy通过；各有1/12/35项环境门控未运行，不算部署覆盖。

本轮真实Linux arm64 Docker专项分别通过：停服日志/强杀恢复38项、PostgreSQL Source凭据恢复37项、迁移非零/缺可执行文件/真实300秒超时/Stop四场景68项、完整owner恢复矩阵98项。每组使用独立自建实例，核验真实HTTP、原任务/操作及物理身份；旧容器已确认移除，卷和数据保留。完整矩阵初轮因fixture未识别受支持的.run别名中断，修正仅接受授权根的固定别名，所有进程/锁/代次保护保留；合法别名反例及越界反例、完整另轮均通过，原失败报告保留。

组件或arm64专项不能代替待构建amd64镜像、K8s部署、平台存储能力或Java透传验收；本机无K8s访问条件。Java的结构化data丢失仍由其负责人处理，见[交接说明](java-error-contract-handoff.md)。
