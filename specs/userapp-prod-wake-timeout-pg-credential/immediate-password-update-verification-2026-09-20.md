# 原地改密验证记录（2026-09-20）

验证时源码基线：`feature-userapp`，HEAD `f5755820` 加本轮修改。实现随后保存为 `c1f8f584`；验证过程没有推送、修改现场数据库或部署。

## 实现

- reset-password 允许运行账号，prod 未运行时明确拒绝，不自动启动。
- 撤除 app-cli 源封存/activation gate，以及平台凭据触发的容器换代和辅助容器实现。
- 普通生命周期不捕获历史保存凭据；AppService/AppState 不再持有该存储接口。
- 退役开发期保存待生效 HTTP 路由；SQL 结果未知、原操作及物理身份保护保留。
- 新 E2E 持有真实 psql TCP 会话，以相同 backend PID 核验改密前后连接；同步严格断言清单。
- 顺带修复原临时提交 process_utils 中 SHA-256 输出 LowerHex 不兼容，改用逐字节等价十六进制编码。

## 已执行

| 命令 | 结果 | 证据 |
|---|---|---|
| `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features` | 退出 0；264 通过，1 跳过 | `/tmp/rcoder-immediate-pg-appcli-nextest.log` |
| `cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets` | 退出 0 | `/tmp/rcoder-immediate-pg-appcli-clippy.log` |
| `cargo nextest run -p rcoder-storage -p app_manager -p rcoder -p docker_manager -p process_utils -p shared_types --no-fail-fast --all-features` | 退出 100；1230 运行，1226 通过，4 失败，18 跳过；四项共享测试夹具已修复，见下行 | `/tmp/rcoder-immediate-pg-nextest.log` |
| `cargo nextest run -p rcoder-storage --all-features --no-fail-fast -E 'test(configuration_tests)'` | 退出 0；18 通过，含前轮全部 4 项失败；159 为筛选排除 | `/tmp/rcoder-immediate-pg-config-recheck.log` |
| `cargo clippy -p rcoder-storage -p app_manager -p rcoder -p docker_manager -p process_utils -p shared_types -p rcoder-e2e --all-targets --all-features` | 退出 0，无 warning；包括修改的 E2E 编译检查 | `/tmp/rcoder-immediate-pg-clippy.log` |

默认 Docker feature 聚焦验证：`cargo nextest run -p app_manager -p docker_manager --no-fail-fast -E 'test(credential) or test(password) or test(configuration) or test(controlled_start)'` 退出 0，9 项通过，294 项筛选排除；证据 `/tmp/rcoder-immediate-pg-default-nextest.log`。根 workspace 与 app-cli 的 fmt check、git diff check 均退出 0。

app-cli 初轮路径反例失败来自 macOS /var 与 /private/var 规范路径比较，修正后全量复跑通过。存储四项失败来自夹具显式 Deploy 后又调用无私有输入的 admit；改为推进已正确受理的记录，保留生产的输入摘要校验，相关 18 项复跑全部通过。未重跑无变更的其余 1226 项，不把分轮验证写成第二轮 1230 全量通过。

## 未验收

- Compose test-e2e 的真实连接、当前容器/owner 不变、dev 隔离场景尚未实际运行；Clippy 编译不代表集成通过。
- 本轮没有 remote K8s、真实 PG 或三平台运行验收。不能引用旧 source-seal 验收作为本轮证据。
- 原交接文档的其他独立 native/发布工作仍按其历史基线记录；本轮未发布 npm 或容器镜像。
