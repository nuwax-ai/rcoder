# 文件服务 API 请求集

此目录提供手动 HTTP 请求和小范围接口检查，用于开发排障。脚本会创建项目、操作 Git 并删除本轮项目，应指向独立测试环境。它们不覆盖完整的 Agent、UserApp 生命周期或部署 E2E。

## 选择服务地址

脚本默认 `http://127.0.0.1:60001`，适用于单独运行的文件服务。当前 Compose 的主 HTTP 服务为 8090，文件服务分流入口为 60000；60001 是 Compose 内部 TypeScript 上游，不直接映射到宿主机。按被测实现显式传入对应地址。

```bash
# 从仓库根目录执行；示例指向 Compose 的文件服务分流入口
bash docker/tests/run-tests.sh http://127.0.0.1:60000
python3 docker/tests/test_file_server.py --base http://127.0.0.1:60000
```

Shell 请求集依赖 Bash、curl 与 Python 3；Python 请求集使用标准库。查看实际失败响应及脚本退出码，不能仅凭汇总数字认定业务验收完成。

## IDE 请求

VS Code REST Client 可打开 `.http` 文件执行请求；JetBrains HTTP Client 也支持此格式。按客户端能力选择 [http-client.env.json](http-client.env.json) 中的本地示例配置，或在客户端私有环境中提供变量：

| 变量 | 含义 |
|---|---|
| `base_url` | 被测文件服务或分流入口 |
| `test_project` | 专用测试项目 ID |
| `test_user` / `test_cid` | Computer 工作区请求的测试身份 |

真实地址和认证信息保存在客户端私有配置或被忽略的环境文件中，不提交到示例文件。执行项目删除等请求前确认目标身份。

## 文件

| 文件 | 内容 |
|---|---|
| `health.http` | 健康检查 |
| `project.http` | 项目创建、查询与删除 |
| `git.http` | Git init/add/commit/status/log/branches |
| `computer.http` | Computer 工作区操作 |
| `build.http` | 构建与开发服务 |
| `run-tests.sh` / `test_file_server.py` | 命令行接口检查 |
| `http-client.env.json` | 客户端示例变量 |

完整回归与严格验收规则见 [E2E 说明](../../tests-e2e/tools/README.md)。
