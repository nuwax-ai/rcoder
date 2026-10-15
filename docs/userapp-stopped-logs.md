# UserApp 停服日志查询

开发容器仍在运行时，业务未启动、启动失败、已经停止，或 app-cli 管理端口暂时不可达，都可以查询磁盘日志。查询不会启动业务、恢复 owner、执行 Stop 或创建/唤醒开发容器。

## 接口

现有地址和请求字段保持：

- `POST /api/v1/userapp/{app_id}/dev/logs/sources/query?user_id=<用户ID>`：日志源及匹配文件。
- `POST /api/v1/userapp/{app_id}/dev/logs/query?user_id=<用户ID>`：快照、日志读取错误和可续拉游标。
- `POST /api/v1/userapp/{app_id}/dev/logs/stream?user_id=<用户ID>`：SSE，携带上次 checkpoint 的 cursor 续拉。

dev 请求转发至同应用已有容器的 file-server，由共享读取器直接读取日志。prod 继续使用原 app-cli 日志管理接口及既有容器唤醒规则。

开发容器已经停止或回收时，接口返回具体的容器不可达原因。本入口不提供容器外历史归档；内存构建任务也不承诺跨容器重启保存。

## 日志与诊断

源清单包含模块日志、构建日志及以下管理源：

| service_id | source_id | 内容 |
|---|---|---|
| app-cli | orchestrator | app-cli 的结构化编排日志 |
| app-cli | owner-recovery | 后备管理进程的 stdout/stderr，包含管理端口开放前的失败 |
| app-cli | management-launch | 平台包装器的启动输出及轮转文件，仅当前应用独占日志目录可读取 |
| app-cli | dev-server | file-server 记录的开发服务控制日志 |

表中的平台源 ID 是默认值。用户已声明同名源时保留其 ID、格式和文件规则，平台源改用 `platform-<原ID>`；该名称也被占用时使用稳定数字后缀。调用方应从源清单选择 ID。模块的 `runtime`、`build` 源采用同样规则，用户声明不会遮蔽平台 stdout 或构建日志。

每服务的用户声明源、单次查询所选用户源总数仍各最多 128 个；平台自动登记的源使用独立、有界的额度，最多 196 个，不挤占用户额度。多服务的声明源总数超过 128 时可选择子集查询，不拒绝整个日志目录。`log-catalog.json` 新增可选来源描述；旧描述保持原指纹校验，不按名称猜测用户源的归属。

发布时配套更新容器内 app-cli 和 file-server 的读取器。旧读取器不能解析带新增来源字段的描述，会保留诊断并使用既有 release 读取路径，不能保证新增平台源可见。只读查询不会改写旧描述。

日志目录与源声明保存为专属状态根中的 `log-catalog.json`。描述仅用于日志发现，不包含环境变量、命令或凭据，不授予运行权限，不参与 owner 或业务启动准入。旧记录缺描述时仅使用已知的当前应用日志目录。

`sources/query` 的元素新增可选 `diagnostic`；快照使用既有 `source_errors`。损坏描述、模块配置和文件读取错误都如实返回，同时保留其他可读的编排器日志。`matched_files=[]` 表示该源尚无匹配文件，不能把权限错误理解为空日志。

SSE 保持 `log`、`source_error`、`source_recovered`、`cursor_reset`、`checkpoint` 和 `heartbeat`。没有 cursor 时默认每源回放末尾 100 行，之后每 500ms 增量读取，15 秒 heartbeat；描述或 release 换代后重置旧游标。客户端断开会取消读取。

读取暂时失败时保留游标及尚未消费的首次 `tail`；读取耗时较长时继续发送 heartbeat，不重启正在执行的读取。

查询失败不应触发自动部署或重启。Java 和前端仍需按当前鉴权与环境规则调用，并保留错误详情；Rust 组件通过不能替代页面联调验收。

## 聚焦真实容器验证

```bash
make test-e2e-userapp-root-logs
```

入口构建当前 Linux app-cli/file-server-proxy，运行隔离容器，验证旧目录启动、停服日志、同容器强杀后重新编译启动及 HTTP。报告记录源码与二进制身份、容器和卷；保留测试卷，不运行完整 E2E。
