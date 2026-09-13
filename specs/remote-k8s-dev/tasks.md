# RCoder 远端 K8s 开发测试任务

- [x] 独立 Make 入口和 dotenv 配置示例，保留既有测试默认值。
- [x] Mutagen 会话管理、忽略规则、flush 和错误检查。
- [x] 清单冻结、内容校验、远端残留隔离与编辑竞争检测。
- [x] 独立 BuildKit builder、缓存、资源预算与三种镜像 digest 报告。
- [x] 自包含部署渲染、资源归属检查、双副本/PG/Ceph/Gateway 配置。
- [x] E2E 套件路由、环境互斥、版本核验和日志收集。
- [x] UserApp 验收扩展到带归属的专属 namespace，保留原默认。
- [x] 本地逻辑回归测试与真实 SSH/Mutagen 预检。
- [x] 远端三种镜像实际构建和推送成功，基础层复用通过。
- [x] 真实 namespace 部署、专属 PG、双副本与 API smoke 通过。
- [x] agent 工作负载和两种 Service 清理、down 保留全部 PVC、恢复核心部署。
- [ ] Gateway 实际请求通过（新旧 Gateway 均超时，待集群网络排查）。
- [ ] UserApp 并发 workspace/归属契约通过（builder Ready，但 owner 回读冲突）。
- [ ] Chat 整轮严格验收通过（三个场景通过，但本地源码漂移令整轮失败）。
- [ ] verify/all 完整验收通过。

实时验证证据与尚未通过项见本目录 README 的验证记录；未通过项不得标记完成。
