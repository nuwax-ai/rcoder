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
