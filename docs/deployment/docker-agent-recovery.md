# Docker agent 重启后的闲置回收

RCoder 主进程重启保留已有计算。Docker inventory重新发现带 `identifier` / `service-type` 标签的普通agent；也按部署配置的 Web 前缀发现缺少管理标签的历史 Running Web agent。名字必须精确回构且服务族无歧义，已有的部分标签必须与名字一致；Computer、UserApp 和 Builder 不走无标签纳管。没有项目登记的实例从首次确认idle重新计时，达到 `cleanup_config.long_idle_timeout_seconds`、至少两轮独立确认，并复核引用和当前活动后按完整CID回收。

自动回收必须确认工作区的持久挂载来源属于当前部署，挂载目标符合原创建布局。Docker socket、配置文件挂载或其他部署的工作区不能作为数据保留证据；未知、只读或被子挂载遮蔽的工作区会保留并输出 `DataProtectionRequired`。不删除卷、不把容器年龄当作idle。

新版agent的 `GetContainerStatus.idle_observation_version=1` 包含Pending准备任务、执行任务及用户监听端口上的已建立连接（包括预览HTTP/WebSocket）。gRPC探测连接本身不保活。Linux读取连接信息失败时返回状态未知。旧agent（证据版本0）缺少连接级证据，但同样参与自动回收：判定以其上报的 `is_active`/`active_tasks` 为准。单节点Docker部署中用户流量均经主服务中转，观察窗口内探活确认无活动即回收；正在使用的实例会因探活Busy保留，被回收的实例在用户下次使用时按需重建，工作区数据保留。

Memory登记的workload锁只在单进程内有效。Docker启动会在配置工作区根持有 `.rcoder-controller.lock`：memory控制器持独占文件锁；共享PostgreSQL准入的控制器持共享锁。第二个memory控制器或memory/PG混用同工作区会启动失败。进程退出自动释放锁，不需要删除锁文件。所有PG副本仍须使用同一权威数据库和准入域。

`cleanup_config.enabled=false` 不执行自动退役；`orphan_reconcile_mode=observe` 只观察。状态探测失败、同CID重启、物理身份变化，以及Docker候选本轮未能完成活动检查，均不累计连续idle。

旧容器缺少创建环境或可验证挂载时仍会保留，名字本身不授权删除。先只读核对完整CID、labels、必要创建环境、网络及mounts；缺证据实例的迁移仍需按具体来源和数据布局处理。不要使用批量 `docker rm rcoder-*` 代替恢复纳管。

旧agent的活动报告不含预览连接级证据；其自动回收依赖观察窗口内探活确认无活动，探活无法感知的旁路长连接会被切断，由按需重建恢复。历史版本曾以 `cleanup_config.legacy_cid_admission` 准入清单放行旧agent，该配置键已移除；旧配置文件中残留的键会被忽略，可直接删除。
