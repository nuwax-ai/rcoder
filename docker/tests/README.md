# file-server API 测试集

## 使用方式

### VS Code REST Client（推荐）
1. 安装扩展：`humao.rest-client`
2. 打开 `.http` 文件，点击 `Send Request` 即可

### JetBrains（IntelliJ / WebStorm）
原生支持 `.http` 文件，无需扩展。

### 命令行
```bash
# 用 curl 批量执行
bash run-tests.sh
```

## 环境变量

在 `http-client.env.json` 中配置（VS Code / JetBrains 自动读取）：
- `base_url`: file-server 地址（默认 `http://127.0.0.1:60001`）
- `test_project`: 测试项目 ID

## 文件说明

| 文件 | 内容 |
|---|---|
| `health.http` | 健康检查 |
| `project.http` | 项目 CRUD + tree |
| `git.http` | git 操作（init/add/commit/status/log/branches） |
| `computer.http` | computer workspace 操作 |
| `build.http` | build/dev server |
| `run-tests.sh` | 命令行批量执行（不依赖 IDE） |
| `http-client.env.json` | 环境变量配置 |
