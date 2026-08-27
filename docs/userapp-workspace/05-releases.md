# 版本化发布与回滚

> **⚠️ 本文 releases 五接口（prepare/activate/confirm/list/delete）已删除
> （2026-08-27 标注）**——部署统一走 `POST /api/v1/userapp/{app_id}/start`（url
> 轻量部署），版本回滚由 start+url 指定历史制品实现。保留作历史参照。

产物名为 `workspace-package-<release_id>.zip`（落 workspace 的 `builds/` 子目录——
build 任务创建时即可从响应/快照的 `artifact_path` 字段拿到确定性相对路径，取包走
`GET /api/v1/userapp/static/{app_id}?release_id=<id>`），根目录必须包含
`workspace.manifest.toml` 和 `release.lock.toml`。

```text
POST /api/v1/userapp/{app_id}/releases/prepare
POST /api/v1/userapp/{app_id}/releases/{release_id}/activate
POST /api/v1/userapp/{app_id}/releases/{release_id}/confirm
GET  /api/v1/userapp/{app_id}/releases
POST /api/v1/userapp/{app_id}/releases/{release_id}/delete
```

prepare 下载到 `.incoming`，校验大小、SHA-256、zip 和 lock 中 release ID，再原子移动到
`packages`。相同 ID+摘要幂等，不同摘要冲突。

activate 可指定任意保留版本。运行中的 workspace 会先停止，包解压到 staging 并完整校验，
`code` 移到 rollback，staging 原子改名为 `code`，然后重启。启动失败会恢复旧 code。
首次发布会进入 `PendingStart`，平台创建计算资源并完成 readiness 后调用 confirm。

默认保留 15 个成功版本，可按应用设置 2–100：

```text
RCODER_APP_RELEASE_RETENTION_DEFAULT=15
RCODER_APP_RELEASE_RETENTION_MAX=100
```

成功 confirm 后才清理；当前版本始终保护。长期只保存 zip，不保留 15 份解压目录。
数据库不执行 down migration，迁移必须幂等并采用 expand/contract。
