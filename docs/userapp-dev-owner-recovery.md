# UserApp dev 管理进程恢复

开发容器被回收后，工作卷里的 app-cli 登记仍可能存在。开发服务的 start、restart、stop 与操作恢复会先检查管理进程，避免让旧登记永久挡住用户。

## 恢复行为

- 原 owner 正常响应：继续核验并复用它，构建前捕获实例与 revision。
- 登记存在但管理端口拒绝连接：尝试 `app-cli serve --control-only --workspace <目录>`。这个入口取得项目排他锁、绑定 API，并完成原有进程清理与 journal 对账，然后等待显式操作，**不会自动启动业务**。
- 新 owner 完成对账后：旧传输登记归档到 `dev-server-external.json` 的 `retired` 字段；原请求保留在历史记录中。归档有快照条件检查，不能删除并发产生的新登记。
- 用户请求停止：向恢复后的 owner 提交 Stop，确认业务子进程退出；管理进程保留供后续操作使用。未确认的部署或迁移历史不会被 Stop 伪造为成功。
- 显式恢复旧操作：仍查询原 operation ID 并核验类型、实例、摘要和结果，不替换为另一个请求重放。

端口拒连仅触发恢复尝试，不证明所有子进程已退出。真正的排他与清理由 app-cli 完成；超时、其他服务占端口或不完整的清理证据不会触发按进程名杀进程、删除 journal 或清空工作卷。

恢复失败会在构建之前返回具体原因，避免先下载依赖再报 owner 不可用。管理面拉起日志位于该应用日志目录的 `app-cli/owner-recovery.log`。身份接口区分初始化、恢复失败及不支持运行控制协议的旧 run 模式。

dev start/restart 的调用方必须先检查响应信封的 `success`；只有成功响应才有可轮询的 `task_id`。管理恢复/预检失败不会创建一个已经注定失败的构建任务。Stop 的成功仍表示业务停止已确认，不等同于刚受理停止请求。

## 运行时边界

- RCoder/file-server 与 app-cli 需要配套更新；旧 app-cli 不认识 `--control-only`，会明确报错，不会静默转回另一条启动路径。
- 容器内 supervisord 管理的子进程，可由新 owner 清理后恢复。新容器进程空间中的 builtin 也使用既有 journal 证据恢复。
- 宿主机 builtin owner 被强杀后，残留子进程的处置仍依赖现有进程树清理证明；本修复不把拒连、超时或 PID 数字当成强杀授权。活着但挂起的 owner 也不自动抢占。
- `/health`、容器 Ready 与应用业务就绪仍然分开。仅查询状态不会拉起业务；本修复没有把业务健康接入容器重启探针。

## Python 模板配套

Python 后端模板的构建脚本将下载缓存放在子项目 `.pip-cache/`，精确版本锁、Python ABI/平台和已安装文件清单一致时直接复用 `deps/`。锁变化或依赖缺失时重新安装，失败保留旧依赖；每次发布重建 ZIP，缓存不进入制品。

现有应用不会因模板更新自动改写。存量项目需同步模板的 `scripts/build-standalone.sh`、`scripts/build-standalone.py` 与 `.gitignore`。镜像安装工具时可使用 `pip --no-cache-dir`，不要设置全局 `PIP_NO_CACHE_DIR` 影响运行期构建。

## 本地回归

使用同一源码构建的 Linux app-cli 和内嵌 file-server-proxy：

```bash
python3 tests-e2e/tools/owner_recovery.py \
  --app-cli /absolute/path/to/linux/app-cli \
  --file-server-proxy /absolute/path/to/linux/file-server-proxy \
  --report tests-e2e/reports/owner-recovery.json
```

该场景运行真实 supervisord、Pingap 和 HTTP 应用，覆盖 owner 强杀、重复停止、再次启动、同卷容器重建与产物态部署。只清理自己创建的容器，保留测试数据卷，报告包含镜像和容器 ID。它验证容器内管理链，不替代 RCoder/Java 全链路或远端 K8s 部署验收。
