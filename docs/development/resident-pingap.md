# UserApp 常驻代理与配置应用

## 生命周期

`app-cli serve/run` 的 owner 持有独立的代理执行范围。业务 Stop、源码 Restart 和热部署只切换业务代次；代理先确认 standby，再允许停止旧业务。正常 Stop 保留入口，返回可重试的 HTTP 503。owner Shutdown 必须确认代理和业务执行范围都已清理，才释放所有权。

builtin 保存真实 Child 和不可变的已确认配置。发现该进程确实退出、旧进程树清理确认后，自动恢复最近已确认的配置；admin 超时、401 或解析错误不会授权另起代理。supervisord 按实际 RUNNING/PID/start/argv、专属配置及所有权回执核验并接续常驻组。

## 配套二进制

本实现需要 Pingap 应用协议 v1，不能与未修订的官方 0.15.0 二进制混用。版本号和上游 commit 仍标识基础版本，RCoder 补丁另有 SHA256；构建及实际二进制回执记录两者。构建入口及固定来源见 [配对构建](../../tools/build/pingap-applied/README.md)。`pingap --apply-protocol-version` 必须输出 `1`；能力不满足时在停旧业务前拒绝。

配置摘要使用解析后的完整配置 JSON：递归按键排序后序列化，再计算 SHA256，不依赖 `serde_json` 的排序 feature。CRC32 hash 仅为一致性字段。

## 发布、查询和失败

- 候选在 `publications/<UUID>/pingap.toml` 中校验并冻结；`validate` 不修改 active。
- 正常配置统一原子发布到 `active/pingap.toml`。`reload` 使用同一 owner 或发布锁，与摘流、恢复串行。
- 确认必须匹配 publication UUID、完整摘要、Applied 回执、实际进程身份和每个监听的 HTTP 标记；正常业务发布还检查声明的业务 HTTP 健康契约。
- `GET /v1/proxy/status` 的 `configured` 表示文件存在；`applied` 表示这次完整应用已确认，包括 standby。二者均不单独表示业务可用。
- 发布失败只在旧图重新确认后返回普通失败；回切结果未知时保留写保护及具体错误。关键读取失败不当作“首次启动”。

Custom 不能自动证明任意业务路由；业务 readiness 继续保留 `custom_route_unverified` 边界。HTTP 标记不能替代页面资源、业务响应或代理路由验收。

## 热载与完整重配置

热载支持 upstreams、locations、plugins 和既有 server.locations。监听、server 集合、basic、证书、storages 及进程级缓存参数变更需要完整重配置；不支持的热请求在停旧业务前拒绝并保留旧业务。

完整重配置使用独立的 owner Shutdown → 新 owner bootstrap 流程：先校验新项目，向捕获的代次执行 `app-cli owner shutdown --workspace <root> --generation <generation> --request-id <id>`；受理响应不等于完成。等待该请求的清理成功和旧 owner 退出后，再用原源码根执行 `app-cli serve/run`。新 owner 必须实际取得系统锁，不删除记录或依靠文件存在判定占用。这一路径允许短暂入口中断，不能称为无中断热载。

不要通过删除状态文件、随意改 PID、关闭确认或手工把操作写成成功来恢复。Stop/Shutdown 与新源码重试均使用原身份和可重试的清理链。

## 验证口径

协议替身只验证 app-cli 的控制、顺序和恢复逻辑。真实 Pingap 图切换、真实 builtin/supervisord 进程，以及 K8s/Gateway 各自记录实际源码摘要和二进制身份；本地组件通过不代表镜像已发布或集群已验收。
