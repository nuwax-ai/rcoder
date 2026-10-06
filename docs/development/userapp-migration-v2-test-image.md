# UserApp 迁移修复：测试镜像构建核对

本轮 Pingap 升级已按用户要求延期，保持 **Pingap 0.14.3、Node 22、Rust stable**。app-cli 源码版本为 **0.3.16**；代码、组件检查、容器验证与发布分别记录，版本号变更不表示已发布。

## 需要使用的新源码

- **RCoder 服务镜像**：包含过期流量唤醒的独立恢复入口及细分错误契约。
- **app-runtime 镜像**：从本轮 RCoder 源码编译 app-cli 和 file-server-proxy。前者包含迁移告警策略、内部运行输入持久化；后者包含启动提交屏障和共享 deadline。file-server-proxy 的版本字符串未改变，须结合源码、构建记录和实际二进制摘要核对。
- **模板/CLI**：默认资源收集、提示词和 skills 已调整，`@nuwax-ai/template-cli@0.1.24` 已发布至 npm（源码提交 `daafbe3c5a074ad59acf4e146b2ca925e1d12c8a`，tag `v0.1.24`）。镜像须安装并核验该版本。使用旧 CLI 初始化的项目不会自动得到新脚本；已有项目需同步构建入口和 helper，重新生成并部署包含运行资源的制品。Restart 不会补齐旧 ZIP 漏掉的文件。

生产构建仓为 `build-agent-docker`。其 app-runtime 构建支持显式 `RCODER_SOURCE_DIR`，可指向本轮实际 RCoder 源码；默认 vendored checkout 不一定包含本轮未提交改动。使用仓库既有构建入口，保留包仓库与平台配置，核对所选源码和构建快照；不要将旧基础镜像内的二进制当成本轮产物。

## 镜像内核对

针对新构建的测试镜像，以独立临时容器读取实际版本和摘要，例如：

```sh
docker run --rm --network none --entrypoint sh "$APP_RUNTIME_TEST_IMAGE" -ec '
  app-cli --version
  file-server-proxy --version
  node --version
  pingap --version
  sha256sum /usr/local/bin/app-cli /usr/local/bin/file-server-proxy
'
```

app-cli 应为 0.3.16，Node 为 22，Pingap 为 0.14.3。二进制摘要按实际平台关联本轮构建记录；macOS 上验证的 Linux arm64 摘要不能替代另一台机器构建的 amd64 摘要。记录镜像 digest 后再部署测试环境，测试期间保持该部署不变。

## 测试重点与边界

迁移退出非零、可执行文件缺失或迁移超时，要保留原日志、任务和操作身份，确认迁移进程树已清理后继续启动业务；真正的业务启动/就绪失败仍报告失败。启动期间 Stop 取消原启动意图，确认业务停止并保留管理 owner，旧回调不能再次启动。未知物理清理或平台数据库写入结果继续保持对应保护。

历史 `traffic_wake_observing` 的恢复须有原启动确认、完整身份、过期 deadline/宽限及条件写入证据；不能按旧标签直接清状态。容器计算重启和业务制品 Restart 是不同入口，见 [Java 接入交接](java-error-contract-handoff.md)。

本机无法访问用户测试 K8s 集群，K8s 部署与存储能力仍未验证。Docker 的隔离场景不能代替 K8s、真实业务及部署验收；镜像构建成功也不能替代服务实际启动与数据保留验证。
