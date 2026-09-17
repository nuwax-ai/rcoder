# build-agent-docker 与 RCoder 配套审查

日期：2026-09-17。镜像仓库 `/Users/soddy/Documents/git-workspace/build-agent-docker`，HEAD `a4a4522`，审查开始和结束工作树均干净。配套 RCoder 本轮基线 `19dfd381`。未改镜像代码、未构建或推送镜像、未连接或修改集群。

检查了近期 managed owner、Pingap 版本、预览协调器 Secret/Deployment、镜像源码 COPY 和 Chart 打包链。此报告补充同目录 review.md 的 R01–R11，不替代它们，也不声称全部镜像与 K8s 模板已经审完。

## B01 / P1：managed 包装脚本命令顺序错误，启用即退出

证据：镜像仓库 `build_config/rcoder-agent-runner/bin/rcoder-app-runtime:29` 执行 `app-cli serve --workspace "$WORKSPACE"`。RCoder `crates/app-cli/src/config.rs:28` 的 workspace 是顶层参数，未声明 global；Serve 没有参数。

只读复现（本机已有 debug 二进制，非新镜像验证）：

```sh
/Users/soddy/Documents/git-workspace/rcoder/crates/app-cli/target/debug/app-cli serve --workspace /tmp/rcoder-review-do-not-create --help
```

退出码 2：`error: unexpected argument '--workspace' found`。解析阶段退出，未创建该目录或启动服务。

修复：使用 `app-cli --workspace "$WORKSPACE" serve`，或统一设计并验证 global CLI 参数。不能仅测脚本 sh -n；必须执行镜像中的真实入口。

验收：managed=1，正确 workspace/token/state_root 条件下 supervisor program RUNNING，身份匹配真实应用，启动操作可提交；普通非 managed 模式不占 3010。

## B02 / P2：默认关闭不是正常 no-op，会启动重试后进入 FATAL

证据：包装脚本 19–21 在默认模式立即 exit 0；`supervisor/conf.d/app-cli.conf:9–13` 设置 autostart=true、autorestart=unexpected、startsecs=3、startretries=3。

Supervisor 在 STARTING 阶段退出早于 startsecs 时，即使 exitcodes=0 也按启动失败处理，经历 BACKOFF 重试并进入 FATAL。不能用 autorestart=unexpected 推导“关闭模式不重试”。这是有限启动重试，不是无限循环。

依据：[Supervisor startsecs 官方说明](https://docs.supervisord.org/configuration.html)。本机未安装 supervisord，未实跑其状态机。

修复优先在启动时按应用模式生成/启用 program，关闭时不注册或 autostart=false；若采用 startsecs=0 的一次性 no-op 设计，明确并测试启用模式的异常重启行为，避免引入新循环。

验收：真实 supervisor 下 managed=0 无 BACKOFF/FATAL、无重复启动日志、无端口占用；managed=1 的 owner 异常退出按预期恢复。

## B03 / P1：managed 开关与真实 workspace/令牌的跨仓注入链未完成

证据：镜像包装脚本 24 默认回退 `/app/userapp-runtime/empty`，16 将 APP_CLI_DEPLOY_TOKEN 描述为可选。RCoder crates 内未找到 APP_CLI_MANAGED 或 APP_CLI_RUNTIME_WORKSPACE 的注入；K8s UserappBuilder 环境构造只增加 APP_CLI_STATE_ROOT（`crates/docker_manager/src/runtime/k8s_agent_env.rs:59` 起）。平台 OwnerClient 写接口需要令牌，并按 workspace 文件名与 PROJECT_ID 核验身份。

影响：默认全关时仍走 legacy，不能用旧模式 E2E 证明 managed 完成。仅手动开启开关但未补 workspace，会让 empty workspace owner 抢占 3010，平台因身份不符拒绝；未配置 token 时 owner 虽可健康响应，平台不能提交运行操作。

修复：在动态 builder 创建链按应用持久模式注入 managed 开关、真实 workspace、应用身份、稳定状态根与可用凭据；Docker/K8s 两条路径都补齐。配置缺失应在占用管理端口前明确拒绝，或采用真正可绑定应用的引导协议，不能用固定 empty 假装已绑定。明确旧实例迁移，不自动按请求回退 legacy。

验收：由 RCoder 真实创建 builder，而非手工 exec 注入；读取身份并经平台执行 source/artifact start/restart/stop；重建保持相同状态根和 Stopped 语义。

## B04 / P1：K8s 显式状态根与平台 token 查找不一致

证据：RCoder `k8s_agent_env.rs:59–68` 为 UserappBuilder 注入 `{USERAPP_DEV_HOME}/{project}/state/{project}`；owner 的 RuntimeStore 优先使用 APP_CLI_STATE_ROOT，token 落该根。`crates/file-server/src/service/dev_server/owner_client.rs:find_owner_token` 只枚举 workspace.parent/.app-cli-state/app 和 workspace/.app-cli-state/app，不读取显式状态根。

影响：即使补齐 B03 并成功启动 owner，平台仍可能报 credentials unavailable。当前“父目录/当前目录各猜一次”不能兼容镜像和运行时的真实路径契约。

修复：跨仓统一状态根/endpoint/token 契约，平台读取权威配置并核验实例，不能靠目录猜测或复制 token 到多个位置规避。错误权限、缺失凭据、双根歧义分别诊断。

验收：使用动态 K8s builder 实际注入的 state_root 完成平台操作；source/.run 切换与容器重建后仍能定位同一 owner，错误凭据不得回退 spawn。

## B05 / P1：预览内部令牌轮换不触发 Pod 更新

证据：`k8s/helm/nuwax-platform/templates/rcoder/preview-secret.yaml` 支持 values 显式 token 覆盖和 lookup 复用；`deployment.yaml:162–166` 用 env secretKeyRef，Pod template 21–24 仅 checksum/config，没有 Secret 轮换标识。

只读 Helm 渲染验证：使用 values-default + values-k8s-test，分别设置两个合成审查令牌；两次 helm template 均退出 0。比较输出：Secret 改变，rcoder Deployment 完全相同。未输出或读取集群真实 token。

影响：单独轮换 Secret 后既有进程仍使用旧环境变量，之后重启的新 Pod 使用新值，跨 Pod 预览控制/转发认证可能不一致。文档的“删 Secret 后 upgrade”也不能保证既有 Pod 重启。

修复：设计基于同一已解析 Secret 值的 rollout 标识或显式轮换 revision，并规定多副本轮换过程。注意 randAlphaNum/lookup 模板若多次独立求值，不可直接天真地 include Secret 模板做 checksum；必须保证 Secret 数据和 rollout 标识来自同一结果。需要无中断轮换时采用双令牌过渡，否则明确受控切换窗口。

验收：新装、无变化 upgrade、显式轮换、已有 Secret 缺 token、回滚；无变化不无故 rollout，轮换后全部副本确实读到新凭据；验证真实跨 Pod 请求。

## 已核对一致的部分

- `makefiles/16-app-runtime.mk:63–64` Pingap=0.14.3，commit=cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8，与 RCoder app-cli Cargo.toml 的 pingap-config rev 一致。源码一致不代表已部署镜像一致。
- agent-runner Dockerfile 独立编译 app-cli 并 COPY 二进制，同时 COPY supervisor/conf.d，因此 B01/B02 确实进入实际镜像配置，而非未使用的样例。
- Chart 打包脚本 `k8s/scripts/merge_chart_values.py` 将 values-default + 对应环境 overlay 烘焙入包内 values.yaml。因此用户有意使用 `helm upgrade ... --reset-values` 获取新版配置的方式合理；本报告不要求额外个人 values，不改这个部署习惯。
- Make 的 update-rcoder / update-rcoder-agent-runner 会拉取配置分支，各 Dockerfile 从对应 code/rcoder COPY；它不会天然使用旁边 RCoder 工作目录的未提交内容。验收应记录各构建上下文的真实提交和镜像 digest，不能只看主工作区 HEAD 或 npm 版本。

## 修复与验证顺序

1. B01/B02：镜像启动脚本和 supervisor 状态机；用真实镜像入口测试开关两档。
2. B03/B04：与 RCoder review.md 的 owner/目录/身份修复一起完成跨仓注入及消费协议。
3. B05：Secret 轮换与多副本认证，先模板对比，再集群请求验证。
4. 使用修复后的 RCoder 源码构建 agent-runner、runtime 与主服务相关镜像；分别记录来源。显式 managed Compose + remote-k8s UserApp + 预览跨 Pod 验收。

本轮不执行镜像发布或集群变更。修复交付中区分本地渲染、镜像启动、集群业务、发布四层，不把模板能渲染称为部署通过。
