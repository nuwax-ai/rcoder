# UserApp 本地 Turso 持久化

当前配置、Make 与原始 Compose 的挂载差异、备份恢复和实例独占要求统一见 [Docker 部署指南：UserApp 元数据持久化](../docs/deployment/docker.md#userapp-元数据持久化)。

Turso 用于本地 UserApp 元数据；Agent 存储独立配置。`make dev-*` 默认叠加 [named volume 配置](docker-compose.turso-volume.yml)，服务重建保留数据。不要通过删除锁、清空数据库或删除数据卷修复启动失败。
