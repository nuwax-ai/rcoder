# CR06：真实 SIGTERM 与在途 UserApp 操作

## 原有覆盖边界

`docker_crash_contract.py` 使用 SIGKILL 检查两个远端副作用窗口的重启隔离；`turso_runtime_contract.py` 通过普通 stop 做离线快照。两者不能证明真实 SIGTERM 期间旧 keep-alive 停止受理、业务任务完成前不关闭存储。

源码关闭顺序：`main.rs` 收信号后关闭 OperationFlightGate、通知生产者、等待 HTTP/proxy 和后台任务；`shutdown.rs` 再等待恢复扫描与业务 guard、flush Activity、关闭 UserApp store。部署测试需要验证这一实际链路，不能仅检查 exit 0。

## 可复用入口

```bash
python3 tests-e2e/tools/docker_shutdown_contract.py \
  --image-metadata /tmp/rcoder-handoff-master-image.json \
  --observer target/debug/userapp-db-observer
```

镜像元数据必须含 tag、image_id、binary_sha256；运行时核验 tag 指向镜像及容器内二进制。observer 必须提前由同 schema 源码构建，脚本不运行 Cargo，拷贝并固定其 SHA256。可通过 `E2E_REPORT_DIR/E2E_RUN_ID/E2E_CASE_ID` 接入启动器；当前尚未修改共享 catalog/run.py。

测试只创建独立 Compose 项目、私有目录和 Docker socket 代理。代理只允许本轮 app/lifecycle 的写入，其他资源不能变更。清理复用 crash fixture 的所属身份核验；私有运行配置删除，日志/数据/断言证据保留。没有操作日常 Compose 或 K8s。

## 必需观测

1. 建立真实健康请求并保留 HTTP/1.1 keep-alive socket。
2. 真实 workspace 操作持久化为 Running，停在 Docker create 发送前屏障；没有创建业务容器。
3. 先核验迟到请求的合法 app_id 当前不存在，再向本轮 RCoder 发 SIGTERM。
4. 确认进程已处理信号；在原 socket 发送新 workspace 请求，不允许客户端隐式重新连接。请求必须被关闭/拒绝。
5. 屏障保持期间原进程仍运行，尚未记录存储与进程关闭完成。
6. 丢弃屏障请求，制造未知远端结果；原操作保留保护，等待 RCoder 自行完成 graceful shutdown 且 exit 0。
7. 重启前用同 Turso 引擎的锁持有 observer 读取数据：目录锁确已释放，原 operation/lifecycle 已是 RecoveryRequired，迟到 app 没有身份/操作记录。observer 不执行迁移或重启隔离。
8. 重启同容器/同数据，原身份仍受保护；原 Docker create 尝试只有一次，未知命令未重放。全部所属资源清理完成。

这验证未知结果的排空/保护，不等同正常成功业务部署、全部 CR06 时序或 PG/K8s 关闭验收。

## 实际执行记录

- 首轮在发信号前因 Python `http` helper 与 `http.client` 重名失败；所属资源清理完成。改用明确模块别名。
- 第二轮真实 SIGTERM、keep-alive 拒绝、排空退出、原身份恢复均通过；最终检查暴露迟到 app_id 超出既有长度限制。修复夹具为等长合法 ID，并增加信号前 `ERR_APP_NOT_FOUND` 前置验证；没有放宽生产校验。
- 第三轮退出 0，12/12 通过，证据：`/private/var/folders/y6/g5lk3d750833hz_rn5h3y6nh0000gn/T/rcoder-sigterm-1z1_5481/docker-shutdown/`。该轮尚未加入重启前 observer 三项断言，不能据此独立证明保护落盘先于重启恢复。
- 第四轮真实 SIGTERM/旧连接/排空退出再次通过，但新增 observer 拒绝读取：现有 observer 二进制内嵌旧 schema checksum，与新 S12 镜像不匹配。按保护契约失败，没有绕过 checksum 或修改数据。报告 `rcoder-sigterm-lvxecq_v/docker-shutdown/`；该轮没有重启恢复，所属容器/卷已清理、数据保留，待同 schema observer 核验后再完整复跑。

被测 master：`dev-master-rcoder:toasty-57035645d26a`，镜像 `sha256:3208b23e8f116fccc144332440869391fa56193364d111f0f1b546b4d75c7de8`，二进制 `57035645d26ad62dcc5ec01a3255a1cdfe3d9c2c09088f0784f3fb8123337fae`。


### 同基线 observer 对第四轮的补查

统一构建 S12 冻结源码的 observer 后，固定副本读取第四轮从未重启的保留数据，退出 0；原操作 `8d226940-028b-4e5e-a547-56512c6f8547` 在关机时已经是 `RecoveryRequired`、`step=creation_result`，生命周期为 `d3980f5d-ec50-46cd-afee-ba5f5626d50d`。因此保护回执不是后续启动 quarantine 写入。证据在第四轮目录 `observer-s12-result.json`，含 observer SHA256。此前失败报告保留原样，不改写为成功；包含该观测的完整第五轮另行执行。


### 完整第五轮通过

退出 0，15/15 必需断言通过，无跳过。证据：`/private/var/folders/y6/g5lk3d750833hz_rn5h3y6nh0000gn/T/rcoder-sigterm-ujxkzer8/docker-shutdown`。该轮包含真正 SIGTERM、原 keep-alive socket 拒绝、屏障期保持在途、关机前保护持久化、离线锁释放、新合法请求零身份、重启原身份保护和单次物理尝试；所属容器及匿名卷已清理，私有配置已删除，原测试数据保留。

原 operation：`2ba71ac5-9174-49bb-95e7-09ba4491648d`；重启前状态 `RecoveryRequired`；observer SHA256：`88164f38b58699bd6730cd0f3841ae4ce716fa7c19487dfec2815be81a81efe2`。脚本与本说明未改共享验收 catalog/run.py，需要将这 15 项名字作为独立冻结断言登记；不把此次 Docker/Turso 证据扩展为 PG/K8s 或正常成功部署关闭验收。


## 严格启动器登记

已在现有 `docker_lifecycle_crash` suite 增加独立 Rust 测试 `docker_runtime_sigterm_drain_contract`，没有增加 group，也没有修改 `run.py`。`contracts.py` 冻结 15 项夹具断言和 1 项 Rust 进程成功断言；`report_identities.json` 使用 scenario 同名、backend `docker-shutdown`。

正常 E2E 输入复用 `E2E_TURSO_RUNTIME_IMAGE` 与 `E2E_TURSO_BINARY_SHA256`，脚本解析镜像 ID 后仍核验实际容器二进制。显式 `--image-metadata` 保留给单独复验，不再要求个人 `/tmp` 元数据文件。

observer 依次选择显式 `--observer`、`E2E_USERAPP_OBSERVER_BINARY`、本报告 `observer/userapp-db-observer` 冻结路径；Rust 包装器在后两者都不存在时，优先使用自身冻结 `run/bin` 同目录的 `userapp-db-observer`；仅 executable 位于 Cargo `deps` 且文件实际存在时回退到上层 profile 目录。缺二进制明确失败，脚本和 Rust 测试均不自动运行 Cargo。所有来源最终复制到本案例目录并记录 SHA256。

注册验证：`python3 -m unittest discover -s tests-e2e/tools -p test_run.py` 24/24，退出 0；Python AST 与 JSON 解析通过；16 个冻结断言逐个对应实际第五轮报告和 Rust 包装器。新 Rust 包装器尚未编译执行，原 15/15 实机证据保持其原基线；后续通过严格启动器执行后追加对应报告。

补充登记修正：`cleanup.py` 精确纳入 `docker-shutdown/ownership.json` 并复用原创建归属校验；对应反例覆盖其他目录不扫描、外来 run 身份拒绝且零 Docker 调用。observer 定位用独立 rustc 夹具执行原函数，4 项真实目录断言通过（冻结同目录、禁止越出冻结目录、Cargo 缺失、Cargo 现成 artifact）；未运行 Cargo。
