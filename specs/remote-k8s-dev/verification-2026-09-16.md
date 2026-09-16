# 远端 amd64 构建与 K8s 验证记录（2026-09-16）

## 本轮结论

已实测通过 Mac 源码同步、Linux amd64 编译、三镜像推送、K8s 按 digest 部署及 smoke。可以使用这条工作流做远端开发验证。

Gateway 实际 HTTP 请求失败；因此不能宣称完整 K8s 功能验收通过。UserApp/Chat 全链路本轮未执行，nextest 不在本工作流的远端构建步骤中。

目标主机、context、namespace、registry 和访问地址均复用 `.env.local`；使用 SSH 密钥，无需写入密码。本轮只操作既有专属环境，未修改共享 Gateway/Cilium/节点网络配置，未删除 PVC。

## 命令与结果

| 命令 | 退出码 | 实际范围 |
|---|---|---|
| `make remote-k8s-doctor` | 0 | Mutagen 0.18.1、SSH、x86_64、Buildx、Ready amd64 节点、存储类、CRD 和 registry 可访问 |
| `python3 -m unittest discover -s tools/remote_k8s/tests -v` | 0 | 11 项工作流测试通过 |
| 首次 `make remote-k8s-verify SUITE=smoke` | 2（Make） | 同步配置指纹变化，正确拒绝复用旧会话，尚未进入构建 |
| `make remote-k8s-sync-stop` / `make remote-k8s-sync-start` | 0 | 按使用说明重建本项目的同步会话 |
| 再次 `make remote-k8s-verify SUITE=smoke` | 0 | 三镜像构建/推送、部署、双副本 Ready、四个 PVC Bound、直连健康接口；环境外资源检查通过 |
| `make remote-k8s-test SUITE=gateway` | 2（Make） | Gateway 健康 HTTP 请求超时，严格报告 fail |
| 独立重试 `make remote-k8s-logs` | 0 | 采集到 Pod、事件和部分日志；其中一个 RCoder 副本的文件日志未生成，不能视为全部诊断项成功 |

## 构建身份

- build_id：`20260916T023335Z-5951e882`
- source_sha256：`84c7d8a15fb214a42dc999cc1d11fdcdc45adfa0118778438788a296e355ae3a`
- 冻结文件数：1490；来自当时含未提交开发修改的工作树，不等于仅 HEAD。
- 三镜像来自同一远端独立复制快照；后续本地修改不进入该快照。
- rcoder digest：`sha256:cd4636ecd64c5fe0cc11015d13d564d63d128e5912741c851f5dd61adc046513`
- computer digest：`sha256:7a971ad7d284d1e625b65c5bd95b973d7dfb3f1d6a9fec5ee9a4b6312bb4cc6c`
- runtime digest：`sha256:b376b7b61538a2de97ed978170372abeb7049fb0b31b27551e447e5f237181b5`
- 编译有 app-cli unused import/mut 警告及 Dockerfile ARG 默认值警告，但构建退出成功；本轮没有修改业务源码消除警告。

证据位于本地忽略目录：

- 构建记录：`.remote-k8s/c188d7e8de407557/builds/20260916T023335Z-5951e882.json`
- 输入文件指纹：`.remote-k8s/c188d7e8de407557/builds/20260916T023335Z-5951e882-source.json`
- 各镜像构建日志：同目录同 build_id 的 `*-rcoder.log`、`*-computer.log`、`*-runtime.log`
- 部署记录：`.remote-k8s/c188d7e8de407557/deployment.json`
- smoke：`.remote-k8s/c188d7e8de407557/tests/4ae12a62c96d4caaa2d575a16e3c2e72/summary.json`
- gateway：`.remote-k8s/c188d7e8de407557/tests/014fb03c0b9a427bb1fce477ebeea54c/summary.json`
- ClusterIP 对比：同 gateway 目录 `cluster-connectivity.json`
- 独立诊断重试：`.remote-k8s/c188d7e8de407557/logs/9c286b5281404581b8bf668e66d32f94/`

## Gateway 与集群连接问题

本轮实际观测：

- Mac → RCoder 直连 NodePort `/health`：HTTP 200。
- Linux → RCoder 直连 NodePort `/health`：HTTP 200。
- Mac/Linux → Gateway NodePort `/health`：5 秒 curl 超时。
- Linux → RCoder Service ClusterIP `/health`：HTTP 200。
- Linux → Gateway Service ClusterIP `/health`：5 秒 curl 超时。
- Gateway Accepted/Programmed=True；HTTPRoute Accepted/ResolvedRefs=True；路由指向本环境 RCoder Service。

由此可确定：镜像运行和直连服务健康，Gateway 状态条件不足以证明转发可用。故障位于 Gateway 访问/转发链路，具体 Cilium/Envoy/网络根因未确定；不能直接归因为业务代码或某项集群配置。

Gateway 失败后的自动诊断也返回 RuntimeError；独立查询节点时复现 `net/http: TLS handshake timeout`。需分别排查控制面间歇性连接和 Gateway 数据路径，不能把两者未经证明归为同一根因。

## 工作流边界与建议

1. `remote-k8s.mk` 是 Python 工作流的入口，命令映射本轮可用，无需为了此次验证改写 Makefile。
2. 构建在 Linux 上执行 Cargo/Buildx；测试由 Mac 启动，向远端 K8s 请求。当前没有“远端 nextest”阶段，不应把编译成功称为 Rust 测试通过。
3. `verify` 会构建并部署新源码；`test` 只对已部署镜像追加验证，务必区分。
4. 继续使用不可变快照和 digest，避免边编辑边构建产生混合源码。userapp/chat/all 还会检查本地测试源码漂移，应冻结测试源码后执行；smoke 不能替代这些业务验收。
5. 同步规则变化时先 sync-stop/sync-start；当前机制是明确拒绝，不是自动更新旧会话。
6. 后续可改善 diagnostics：各采集项独立记录失败、保留具体错误，避免首个控制面错误导致整份诊断缺失。不要用放宽测试或绕过 Gateway 使全链报告通过。

本轮保留专属部署、镜像回执和同步会话，便于继续排查。测试环境没有执行 down，数据卷保留。
