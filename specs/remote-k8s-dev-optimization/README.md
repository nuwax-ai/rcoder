# Remote K8s 开发验证体验优化 — 使用说明

状态：批次 A+B 已实现（提交 `511c5c1b`）。本文件是新入口的用法速查；完整验证证据见本目录 `verification.md`，需求见 `spec.md`。

## 新命令

```bash
make remote-k8s-status                 # 只读：已部署身份/入口/实时匹配/最近测试（历史结果明确标记）
make remote-k8s-check                  # 十项分层诊断：ssh/API/ns/deployment/直连/Gateway/PVC/SC/coredns/ceph
                                       #   pass|fail|unknown + 耗时 + 整体预算；报告落 .remote-k8s/<环境ID>/checks/
make remote-k8s-verify SUITE=smoke     # 构建部署测试一轮：冻结一次，构建与测试消费同一份输入
make remote-k8s-test SUITE=chat CASE=<已注册场景名>   # chat 单场景精确筛选（部分覆盖，显式标记）
make remote-k8s-retest-failed RUN=<父测试ID>          # 复用父报告冻结快照逐失败用例重跑
```

## 关键行为变化

- **测试输入冻结（R1）**：`test`/`verify` 在开始时把当前工作目录（含未提交修改，凭据/输出排除）封存到 `.remote-k8s/<环境ID>/test-snapshots/<id>/`；验收执行快照内副本，期间继续编辑源码不影响本轮（活动目录变化仅记提示）。快照被修改/新增/删除/链接逃逸 → 立即失败。
- **同轮输入（verify）**：`verify` 冻结一份清单，远端构建快照（`snapshot.create(expected=…)`）与本地测试快照必须与它逐字一致，否则明确失败。
- **构建复用（R2，保守整源）**：同一（源码摘要 + 三基础镜像 digest + 工具链 + 构建参数 + 平台）的完整成功产物直接复用，registry 内逐镜像 digest 核验后才命中；产物缺失正常重建，registry 故障显式报错。receipt 记录 `cache.reused_from` / `cache_key` / 分阶段耗时。源码任一文件变化即 miss（文档免编译属 C 批，未做）。
- **CASE 语义**：仅 `SUITE=chat` 支持，精确匹配已注册场景；UserApp 是有依赖的完整生命周期链，未建独立 fixture 前不允许截断。筛选通过显式标记 `partial_coverage`，不能算全套通过。
- **参数安全**：`SUITE`/`CASE`/`RUN` 经 make 解析期 `export` 传入 Python（recipe 零 shell 插值），Python 侧对已知套件/已注册场景/32-hex ID fail-fast。

## 启动器显式输入模式（无参数时旧行为不变）

`tests-e2e/tools/run.py` 与 `k8s_userapp.py` 识别以下环境变量；全部缺省时与旧版逐字相同：

| 变量 | 含义 |
|---|---|
| `E2E_SOURCE_ROOT` | 源码根（快照目录，无 .git 也可运行） |
| `E2E_INPUT_MANIFEST` | 冻结输入清单 JSON（指纹 = 清单本体） |
| `E2E_ORIGIN_HEAD` | 历史基线 HEAD（仅溯源；本轮身份是清单摘要） |
| `E2E_RUN_ROOT` | 报告根（默认 `tests-e2e/reports`） |

## 已知边界

- 细粒度目标级缓存、文档变更免编译、Cargo mtime touch 优化 → C 批未做。
- `retest-failed` 仅处理有可靠场景级结果的失败；构建/环境/aborted 失败要求修复后重跑原入口。
- `status`/`check` 只读，不创建资源；`check` 无权限项记 unknown。
