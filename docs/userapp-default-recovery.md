# UserApp 保留存储恢复

K8s 模式下，UserApp 的工作负载被删除，但原应用 PVC 仍存在时，RCoder 可从保留存储恢复身份和容器。开发环境通过原开发请求或 dev 重启入口重建 builder；生产环境通过访问流量或原重启入口恢复服务。

## 开关

配置文件：

```yaml
userapp_storage:
  default_recovery_enabled: true
```

环境变量优先：

```bash
RCODER_USERAPP_DEFAULT_RECOVERY_ENABLED=true
```

默认开启。设为 `false` 关闭本次新增的默认模板兜底，已有正常身份、配置和运行时的常规生命周期能力继续工作。Compose 可使用 `${RCODER_USERAPP_DEFAULT_RECOVERY_ENABLED:-true}`；远端 K8s 验证工作流对应 `REMOTE_K8S_USERAPP_DEFAULT_RECOVERY_ENABLED`。

## 恢复行为

- PG 正常可用但应用身份缺失时，从受管理且归属一致的 PVC 和稳定 Service 恢复身份。PG 连接或查询失败会返回实际错误，不把数据库故障当成缺少记录。
- 已无工作负载时，使用当前运行时模板重建计算资源，复用原 PVC 的 UID、存储类和容量。可读取的已完成操作配置保留业务环境变量及资源设置；当前显式请求参数优先。无可用历史配置时使用默认值。
- dev 恢复工作区及管理服务，后续仍使用原 `app-cli run` 完成当前源码的编译、启动。
- prod 由新版本 app-cli 从保留卷中的已发布运行配置和制品恢复业务，不重新下载旧 URL，也不自动构建发布 dev 源码。卷内制品、运行配置或必要凭据缺失时如实报错；容器就绪不代表业务恢复成功。
- 旧 PVC 缺少生命周期标注时，先核验规范名称、管理标签、资源 UID 和实际消费者，再在操作租约下以 CAS 补齐标注。不会重新创建或删除原数据卷。
- 并发请求共用原恢复操作。旧版本明确的只读身份检查失败，可在确认计算资源不存在且无活动执行者后收束为失败，原入口继续重试；实际执行或未知写入仍需要对账。

## 边界与验证

应用明确删除、资源归属冲突、PVC 被替换、仍有其他 Pod 使用卷等情况不会自动接管。该能力不能恢复已丢失的持久数据，也不提供 PG 不可用时的无数据库运行模式。仅修改开关不能让旧版 RCoder/app-cli 获得恢复能力，需要部署含实现的新版本。

隔离环境验证入口：

```bash
make remote-k8s-verify SUITE=smoke
make remote-k8s-test SUITE=userapp
```

UserApp 套件追加保留卷恢复场景：构建并发布 A，删除本场景的工作负载及 PG 身份、移除旧 PVC 标注，通过 dev 重启恢复后构建 B，再以并发 prod 流量验证仍服务 A，并检查数据、PVC UID 和单实例。故障注入只作用于验证器创建的专用应用与 namespace。场景保留其 PVC 作为证据，不清理共享存储。
