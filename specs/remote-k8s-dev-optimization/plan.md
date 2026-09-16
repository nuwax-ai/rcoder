# Remote K8s 优化技术方案

## 1. 模块边界

沿用 `tools/remote_k8s/`，按职责拆分 `test_snapshot.py`、`build_cache.py`、`diagnostics.py`（可按实际组织调整），main.py 只编排。保留环境锁与 remote_process 的断连处理；独立环境不得共享可并发写的 Cargo target。

## 2. 测试快照与输入契约

建议目录：

```
.remote-k8s/<环境ID>/
  test-snapshots/<snapshot-id>/source/
  test-snapshots/<snapshot-id>/inputs.json
  tests/<test-id>/summary.json
  tests/<test-id>/details/
  cache/builds/<cache-key>.json
```

冻结算法：

1. 使用现有 manifest 的白名单及凭据排除规则获取输入，包含未提交和未忽略的新增文件。
2. 复制到新目录，保留执行位；符号链接只能指向快照内部，不得硬链接活动工作目录。
3. 校验复制内容与清单，并重新检查输入端是否在冻结期间变化；失败清理本轮未完成目录，有限重试。
4. seal 完成后才允许运行。环境配置通过已加载、筛选后的 env 传入，不复制 `.env.local`。
5. 执行器的 cwd、脚本路径、Cargo manifest、fixtures 全部指向快照。结果写独立目录；执行前后核查输入集合和值，捕获新增/删除/替换。

### 避免 Git 与指纹陷阱

当前两个启动器依赖 `git rev-parse HEAD`、`git ls-files` 及相对路径，不能只换一个脚本路径就认为完成。

优先让启动器支持显式 source root、输入清单、origin head、report root；保留无参数时旧行为。origin head 只是历史基线，不能代表未提交内容；摘要才是本次身份。

不建议 `git clone --shared` 作为固定快照的长期依赖：它引用活动对象库，源仓库 GC 后可能损坏；新目录的 index、ignored/untracked 行为也容易漏检。若必须建立独立 Git 元数据，应自包含且不触碰用户仓库，证明所有文件枚举语义一致，不要求用户提交代码。

快照封存要排除明确的输出目录（reports/target/__pycache__ 等），不能忽略整个任意子目录来隐藏输入变化。只检查“旧清单里的文件”不足以发现新增的可执行输入。

### 源码版本选择

- `verify` 默认让构建和测试消费同一轮冻结输入；构建后用户继续编辑不改变该轮验证对象。
- `test` 默认可冻结当前测试输入来验证已部署版本，但展示 server_source/test_source 两套身份；可提供显式复用部署关联测试快照模式。
- `retest-failed` 必须复用父报告的冻结测试输入。快照丢失或不可信时拒绝，不能偷偷改用最新源码。

## 3. 构建复用

cache key 至少覆盖：schema版本、输入manifest摘要、三种基础镜像digest、Rust构建镜像digest、目标平台、Dockerfile/脚本、Cargo锁文件/features/profile、影响产物的build args与镜像目标。

步骤：

1. 先补阶段计时，建立冷/热构建与无改动重复执行基线。
2. 保守全输入缓存：只接纳完整成功receipt；verify依然检查部署身份并执行测试，cache hit不等于test pass。
3. 检查registry产物存在且digest符合记录后复用；按阶段记录网络/权限/不存在错误。
4. 按 rcoder/computer/runtime 输出闭包细分缓存，不能因只改 app-cli 就错误复用依赖共享crate的镜像。
5. 文档变更免编译的目标必须建立输入合同：构建上下文与key使用同一manifest；未知输入默认失效。添加反例证明 build.rs/include_str 引入的非Rust文件会触发失效。
6. 最后再处理 Cargo mtime：保留现有 touch 保护，直到新增/修改/删除文件、Cargo.toml/lock、feature变更的真实构建实验均证明新机制正确。

可先交付保守缓存，后交付目标级缓存；在 tasks 标明细粒度阶段未完成，不能把它算作已达成。

## 4. 命令接口（目标设计，当前未实现）

```bash
make remote-k8s-status
make remote-k8s-check
make remote-k8s-verify SUITE=smoke
make remote-k8s-test SUITE=chat CASE=<已注册场景名>
make remote-k8s-retest-failed RUN=<父测试ID>
```

- CASE 初期建议精确匹配，`list-cases` 或 status/help 展示支持清单；如果保留子串语义，须展示实际选择并拒绝空集。
- 不把 CASE 直接拼进 shell。当前 Make 配方直接插值参数，应一并检查引号/命令替换注入；建议通过环境传递到 Python，Python构造 argv，远端继续 shlex.join。
- CASE 不支持的 suite/动作提前拒绝；完整 userapp 行为不变。
- fail-fast 指执行前尽早报配置/选择错误，不代表整轮失败后放弃收集已有结果。
- retest读取ID必须校验格式与目录边界，不接受任意路径穿越；明确区分case failure和infrastructure failure。

## 5. 状态与诊断

status 展示配置与现场入口、Pod UID/imageID、当前部署是否匹配receipt、最后报告及来源，不读取/打印Secret或容器env。

check 和 logs 输出统一分项结构：name、status(pass/fail/unknown)、duration、error_class、证据路径。独立读操作可以有限并行；变更、环境测试、构建仍遵守锁。

健康检查可验证两节点Gateway与直连、API延迟/错误、Pod重启、StorageClass/PVC、Ceph健康详情。Ceph查询无权限时明确unknown。Gateway健康通过不能替代动态路由与WebSocket/SSE业务验收。

环境正常但LLM缺配置属于对应AI套件前置失败，不能让不使用LLM的smoke受到不必要阻塞。外部资源合法维护后用明确重新部署或显式审计基线流程恢复，不自动吞漂移。

## 6. 分批实施及验收

A：固定快照 + 计时 + status/check + 诊断完整性。
B：保守缓存 + 安全场景筛选 + 失败重跑。
C：经证据支持的细粒度缓存；按需求再做watch。

各批先聚焦Python测试，再在个人集群跑实际流程。长期测试时原目录编辑不应影响已冻结轮次，但修改冻结目录或替换部署必须被阻止。真实UserApp/Chat如果发现业务问题，保留失败证据，不擅自扩大业务修复或改变断言。
