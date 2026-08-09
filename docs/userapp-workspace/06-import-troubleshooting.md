# 导入已有项目与排错

```text
POST /api/userapp/projects/detect
POST /api/userapp/projects/confirm
```

请求提供 `appId` 和 workspace 一级 `projectDir`。detect 只读取 package.json、pom.xml、
go.mod、pyproject/requirements、Cargo.toml，不执行脚本，生成
`project.manifest.draft.toml`。draft 不参与构建。

开发者必须确认 build argv、zip 产物、run argv、健康路径、代理和日志 glob，再调用 confirm。
confirm 严格校验并原子改名为 `project.manifest.toml`。

常见失败：

- “unsupported schema”：必须显式 `schema_version = 1`，旧格式不兼容。
- “unknown field”：字段拼写错误或仍在使用 `cmd/cache/rate_limit/[deploy]`。
- “dependency cycle”：`depends_on` 必须是有向无环图。
- “no files match”：应用未写到 `APP_LOG_DIR`，或声明的 glob 不匹配。
- “pingap -t rejected”：插件参数错误或运行时 Pingap 版本不兼容。
- “release lock ID mismatch”：下载 URL 指向了其它 release 的包。
