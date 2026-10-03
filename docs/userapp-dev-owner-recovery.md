# UserApp dev 管理进程恢复

开发容器被回收后，工作卷里的 app-cli 登记仍可能存在。开发服务的 start、restart、stop 与操作恢复会先检查管理进程，避免让旧登记永久挡住用户。

## 启动目录与登记纠偏

普通 CLI 按 `--workspace`、`APP_CLI_WORKSPACE`、启动时工作目录的顺序选择工作区。例如在项目根运行 `app-cli serve`，或通过 `app-cli serve --workspace /path/to/project` 显式指定。平台调用始终传入原始源码根，不随 agent 后续切换目录改变。

UserApp dev builder 的 `APP_CLI_RUNTIME_WORKSPACE` 为 `/home/user/{app_id}`，启动包装器将它传给 `serve --workspace`；状态根继续为该源码根下的 `state/{app_id}`。prod 使用独立的部署目录配置。

平台托管的同应用登记若仍指向源码根内的旧子目录，会先按原实例与代次核验清理。活 owner 使用精确 Shutdown 释放所有权；旧 owner 已退出时，由新管理进程完成原代次恢复。确认清理后才发布正确根，继续当前请求。这个流程保留原记录与业务文件，不搬迁项目，也不重放结果未知的迁移。其他应用、越界目录、未确认的清理或无法核验的身份仍返回具体原因。

源码模式的启动与重启会先按当前 manifest 和运行元数据核验派生的 `release.lock.toml`，再检查 owner 能力及启停服务。损坏或输入已变化的锁文件自动原子重建；有效且内容一致时保留原字节与 release ID，不因重复请求改变迁移身份。新的配置无效或 owner 不支持新能力时，在停止旧服务之前报错。制品模式继续验证指定制品中的锁文件，不用源码重建绕过制品校验。

配套升级需更新 RCoder 与 builder 内的 app-cli，并使存量 builder 使用新的启动目录环境。修改模板提示词、升级 npm 包或只更换主服务，均不代表存量容器已使用新目录和二进制。

## 恢复行为

- 原 owner 正常响应：继续核验并复用它，构建前捕获实例与 revision。历史传输请求不再阻止新构建；新请求替换本地传输槽，旧请求仍可查询，迟到响应不能清除新登记。
- 登记存在但管理端口拒绝连接：尝试 `app-cli serve --control-only --workspace <目录>`。这个入口取得项目排他锁、绑定 API，并完成原有进程清理与 journal 对账，然后等待显式操作，**不会自动启动业务**。
- 新 owner 完成对账后：旧传输登记归档到 `dev-server-external.json` 的 `retired` 字段；原请求保留在历史记录中。归档有快照条件检查，不能删除并发产生的新登记。
- 用户请求停止：先向 owner 提交 Stop；3 秒内未完成则通过独立监督器停止捕获的旧执行代次。app-cli 的优雅退出上限为 3 秒，不自动延长。确认业务子进程退出后，管理进程保留供后续操作使用。未确认的部署或迁移历史不会被 Stop 伪造为成功。
- 显式恢复旧操作：仍查询原 operation ID 并核验类型、实例、摘要和结果，不替换为另一个请求重放。

端口拒连仅触发恢复尝试，不证明所有子进程已退出。真正的排他与清理由 app-cli 完成；超时、其他服务占端口或不完整的清理证据不会触发按进程名杀进程、删除 journal 或清空工作卷。

损坏的传输登记与已退役 owner 的操作记录自动备份原件并重建，不要求用户删除状态目录。正在执行的请求不会被这条恢复路径误标终态；磁盘不可写等真实存储故障仍返回具体原因。未提交结果的旧请求记录为失败，不伪造迁移回滚或成功。

容器停止回收后，同一平台和工作卷的新容器拥有新的物理实例身份。app-cli 保留旧容器的进程记录供诊断，只恢复和停止当前容器的进程，不以旧 Pod 是否还能查询到作为启动条件。旧容器未完成的 Shutdown 也不会被重放到新容器。RCoder 负责容器替换、卷使用和旧物理资源回收；相同容器内的活进程排他与停止确认继续有效。

管理初始化和并发拉起会有界等待；已确认归属的管理面保护状态通过独立监督器恢复后再构建。恢复失败会在构建之前返回具体原因，避免先下载依赖再报 owner 不可用。管理面拉起日志位于该应用日志目录的 `app-cli/owner-recovery.log`。身份接口区分初始化、恢复失败及不支持运行控制协议的旧 run 模式。

dev start/restart 的调用方必须先检查响应信封的 `success`。已确认应用作用域的管理恢复/项目预检失败仍立即返回失败信封，并附带已是 Failed 的诊断任务 ID，供现有 GET/SSE 回查；任务容量不足时 `task_id=null`，参数或权限拒绝不注册任务。诊断任务不占执行名额、不触发构建，接入与保留边界见 [项目与任务诊断](userapp-project-diagnostics.md)。Stop 的成功仍表示业务停止已确认，不等同于刚受理停止请求。

## 运行时边界

- RCoder/file-server 与 app-cli 需要配套更新；旧 app-cli 不认识 `--control-only`，会明确报错，不会静默转回另一条启动路径。
- 容器内 supervisord 管理的子进程，可由新 owner 清理后恢复。新容器进程空间中的 builtin 也使用既有 journal 证据恢复。
- 宿主机 builtin owner 被强杀后，残留子进程的处置仍依赖现有进程树清理证明；本修复不把拒连、超时或 PID 数字当成强杀授权。带有独立监督回执的挂起 owner 通过监督器收束并重建管理面；没有监督回执的旧进程不按名称或端口强杀。
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

完整的闲置回收链使用 `make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_idle_recycle_owner_recovery`。它让隔离 RCoder 的真实清理器销毁 builder，保留旧 owner/journal 后再通过 RCoder ensure、Stop、构建 Start、重复 Stop、构建 Restart，并核验 HTTP 新内容和构建计数。配置与镜像前置见 [E2E 场景说明](../tests-e2e/tools/README.md#闲置回收后的-owner-恢复与重新构建)。该命令是验收入口，实际通过情况以对应报告为准。

## 旧 builder 登记的显式修复

早期版本可能已创建新 StatefulSet，但仍保留旧控制器 UID 的 PostgreSQL 项目登记，且没有运行时创建回执。正常 ensure 继续保留创建身份检查；此类登记通过现有显式采用接口修复：

```bash
curl -X POST "$RCODER_URL/api/v1/userapp/$APP_ID/builder/adopt" \
  -H 'Content-Type: application/json' \
  --data '{"lifecycle_id":"<当前应用生命周期>","request_id":"<本次修复的稳定请求身份>","expected_container_id":"<当前 builder Pod UID>"}'
```

调用方须按当前环境提供鉴权。`lifecycle_id` 使用平台当前登记，`expected_container_id` 是实时查询的 Pod UID；不能使用 Pod 名称、旧 UID 或其他生命周期。相同请求重试沿用原 `request_id` 和请求内容。

接口重新核验 StatefulSet、Pod 所有权及当前 PVC UID，并在同一短事务中检查旧登记的项目/容器代次和 revision、旧 UID 的生命周期绑定、当前控制头，然后提交当前绑定、项目登记和本次 Adopt 结果。当前 UID 已有同生命周期绑定时保留其原操作身份；历史 Ensure 结果保持原样。源登记已变化、生命周期冲突、PVC 正在删除或身份读取失败时返回具体错误。

这次修复只更新登记：不重建业务容器、不删除 PVC、不改变停止意图，也不制造旧进程退出或历史 PVC 连续性的证据。之后的 Stop/Restart 仍通过原运行时租约和执行域核验。当前出口支持 PostgreSQL 持久登记和已有 Pod；没有旧生命周期绑定等证据缺口仍需先恢复归属，不能按同名资源自动接管。
