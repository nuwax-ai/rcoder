# Dev 手动 app-cli owner 停止修复

## 现场与接口边界

现场的 agent 在应用工作区内 `.local-deploy` 启动 app-cli，file-server 没有该 owner 的内存登记，因而拒绝 dev/stop。这与 prod 的旧操作租约是两个问题。

`/userapp/dev/stop` 停止应用服务，保留 app-cli 控制进程和 Pod；`/computer/pod/stop` 控制计算资源。两者不能混为一谈。

## 实现

1. 无登记时读取 owner identity。仅连接明确被拒绝表示无监听；超时、503、无效响应必须返回诊断，不能报停止成功。
2. Stop 核验协议、application_id、service_family、runtime_instance_id 和项目真实路径。旧版手工构建目录位于该应用工作区内时允许停止；canonicalize 排除指向其他工作区的符号链接。不按端口或进程名杀进程。
3. 工作区外的本地构建目录必须有明确来源。app-cli `build --deploy-dir` 写入 `.app-cli-project-origin.json`，记录实际项目与运行目录；并发写有文件锁。复制到不同目录或损坏的记录不能授权接管。
4. 来源记录只证明项目归属，不改变锁目录、token、journal 的定位。Stop 对工作区内子目录的授权不用于放宽 Start/Deploy 的源码目录匹配。
5. 核验后从 owner 的真实运行目录查找已有 token。停止意图在 HTTP 提交前持久化；未知响应保留原操作身份，确认成功才移除登记。
6. file-server 重启后重新核验原 runtime_instance_id，并从原运行目录恢复 token，避免自定义目录下找错凭据。
7. owner 确认服务停止时响应 `Stopped`，不因没有由 file-server 杀掉的 PID 而误报 `No running process found`。

没有新增 token 服务或要求用户设置环境变量。app-cli 现有实现会生成并保存控制 token。

## 验证与部署要求

组件反例覆盖：旧版无来源记录的 `.local-deploy`；失败后重建 file-server manager 再停止；认证头；成功不杀 owner；503 不假成功；外部目录、符号链接越界、来源记录复制或损坏；新 build 写入来源记录。结果与命令记入 verification.md。

尚需实际 Compose 和 K8s 验收：在干净应用内由命令行启动 owner（不经过 dev/start），经 Java/RCoder 请求 dev/stop，核查服务已停、owner 3010 仍可用、Pod UID 与 PVC 不变，然后显式启动成功。工作区内旧 `.local-deploy` 停止仅依赖新 file-server；新 app-cli 的来源记录用于工作区外构建目录。

本轮不修改 .18 部署，不清理现场状态，不把组件测试称作线上已修复。

## 本轮继续推进的容器控制

控制操作认领时，同一短事务收束它所中断、尚未被认领的 Pending 业务操作。条件是 revision=1、executor 为空、无物理租约、生命周期和槽位匹配；保留原失败历史与请求身份，记录取消原因及 compute operation id。已运行、等待重试或 RecoveryRequired 不通过该路径清理。

这只解决“尚未执行却永久占槽”的分支。运行时在途写收束、dev/prod 控制执行器、恢复入口、自动登记恢复与部署 E2E 仍未完成，见 tasks.md。


### 后续显式启动的目录边界

手动 owner 若运行在自定义构建目录，停止完成后它仍持有3010。平台想切回源码目录执行 Start 时，不能把来源记录当作已经切换执行目录的证明；Start/Deploy 保留原核验。该跨目录 attach/执行目标切换仍需实现与独立反例，当前不宣称整条“手工启动→停止→平台切源码启动”已通过。
