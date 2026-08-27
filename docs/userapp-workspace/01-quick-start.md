# 快速开始与目录约定

```text
workspace/
├── workspace.manifest.toml
├── pingap/                         # extend/custom 时可选
├── frontend/project.manifest.toml
├── backend/project.manifest.toml
└── imported/project.manifest.draft.toml
```

只有一级子目录中的正式 `project.manifest.toml` 会参与构建；draft 不参与。

构建：

```http
POST /api/v1/userapp/build
Content-Type: application/json

{"app_id":"app-example","user_id":"u-1"}
```

响应（HttpResult 信封）`data` 含 `task_id` 与预生成的 `artifact_path`；轮询
`GET /api/v1/userapp/tasks/{task_id}` 到终态后，快照含 `release_id`、SHA-256、
产物大小与文件名。构建命令直接按 argv
执行，不经过隐式 shell；需要 shell 功能时显式配置 `["sh","-c","..."]`。

file-server 构建前必须配置 `RCODER_PINGAP_VERSION`、`RCODER_PINGAP_COMMIT` 和
`RCODER_RUNTIME_IMAGE_DIGEST`。缺失时 Fail Fast，禁止生成身份为 `latest/unknown` 的锁文件。
为兼容 Manifest v1，第三个变量沿用旧名称，但值是与平台 Chart 版本绑定的完整镜像引用
（例如 `registry/nuwax-k8s-test/app-runtime:0.1.140`），不要求解析 registry manifest
或填写 `sha256` digest。
发布激活后，app_manager 会从 `release.lock.toml` 将这三项作为保留环境变量注入
UserApp 容器；应用请求和项目 manifest 都不能覆盖它们。
应用容器启动时会用同名环境变量复核 release lock，任何不一致都会保持 not-ready。

运行时固定目录：

```text
/app/code
/app/data
/app/logs/<service_id>
/app/releases/{index.json,packages,.incoming,.staging,.rollback}
```

模板必须从 `PORT` 读取内部端口，从 `APP_LOG_DIR` 读取日志目录。不得覆盖
`PORT`、`HOST`、`HOSTNAME`、`APP_LOG_DIR`、`APP_SERVICE_ID`、`APP_RELEASE_ID`
或 `RCODER_*` 环境变量。
