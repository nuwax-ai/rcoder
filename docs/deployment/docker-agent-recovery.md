# Docker agent 重启后的闲置回收

RCoder 主进程重启保留已有计算。Docker inventory重新发现带 `identifier` / `service-type` 标签的普通agent；没有项目登记的实例从首次确认idle重新计时，达到 `cleanup_config.long_idle_timeout_seconds`、至少两轮独立确认，并复核引用和当前活动后按完整CID回收。

自动回收必须确认工作区的持久挂载来源属于当前部署，挂载目标符合原创建布局。Docker socket、配置文件挂载或其他部署的工作区不能作为数据保留证据；未知、只读或被子挂载遮蔽的工作区会保留并输出 `DataProtectionRequired`。不删除卷、不把容器年龄当作idle。

新版agent的 `GetContainerStatus.idle_observation_version=1` 包含Pending准备任务、执行任务及用户监听端口上的已建立连接（包括预览HTTP/WebSocket）。gRPC探测连接本身不保活。Linux读取连接信息失败时返回状态未知；旧agent没有此证据时保留并提示升级。升级RCoder主镜像与agent镜像后，旧实例仍需沿已有受控恢复/替换流程切换到新agent，不能强制删除正在使用的旧实例。

Memory登记的workload锁只在单进程内有效。Docker启动会在配置工作区根持有 `.rcoder-controller.lock`：memory控制器持独占文件锁；共享PostgreSQL准入的控制器持共享锁。第二个memory控制器或memory/PG混用同工作区会启动失败。进程退出自动释放锁，不需要删除锁文件。所有PG副本仍须使用同一权威数据库和准入域。

`cleanup_config.enabled=false` 不执行自动退役；`orphan_reconcile_mode=observe` 只观察。状态探测失败、同CID重启、物理身份变化，以及Docker候选本轮未能完成活动检查，均不累计连续idle。

旧容器缺少身份标签、创建环境或可验证挂载时不会按名称猜测删除。先只读核对完整CID、labels、必要创建环境、网络及mounts；缺证据实例的迁移仍需按具体来源和数据布局处理。不要使用批量 `docker rm rcoder-*` 代替恢复纳管。
