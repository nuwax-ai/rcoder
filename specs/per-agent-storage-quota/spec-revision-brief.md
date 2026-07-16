# Spec 修订说明:per-agent-storage-quota(2026-07-15 EDQUOT 事故根因 + 修改指引)

> 本文是给**执行 agent**的上下文 + 修改指令,不依赖任何外部对话即可独立执行。
> **目标**:修订 `specs/per-agent-storage-quota/per-agent-storage-quota-spec.md`,把"配额方案失败根因"从模糊表述修正为精确归因,并补强方案选型依据,避免接手者再走 kernel-mount + client-setfattr 的回头路。
> **原则**:保留 spec 现有结构与已评审决策(§7),只修正"根因归因"相关的表述 + 补充依据,**不改方案结论、不改代码、不部署**。
> **依据**:Ceph 官方文档 + rcoder 代码注释 + `docs/BACKLOG.md` §待办2。

---

## 一、背景:2026-07-15 线上 EDQUOT 事故还原

rcoder 创建 agent 时,曾用 `xattr::set(子目录, "ceph.quota.max_bytes", N)` 给每个用户/agent 的 CephFS 工作区**子目录**设磁盘配额(代码 `kubernetes_runtime.rs:742-768`,现已注释禁用)。事故时间线:

1. **旧 caps 时期**:rcoder pod 挂共享 CephFS 根,cephx caps = `allow rw`。每次 `create_container` 都执行 `xattr::set(quota_dir, "ceph.quota.max_bytes", 10Gi)`。在那个 caps × kernel × ceph 版本组合下,虚拟 xattr 写入**被意外放行 → 设成功**,日志打 `info!("CephFS quota set ... = 10Gi")`,看似正常。
   - ⚠️ 代码是**盲设**(`:753-763` 只有 `Ok→info / Err→warn`,无 getfattr 读回校验)。
   - ⚠️ kernel client 的 `getfattr` **读不回** `ceph.quota.*`(见 `docs/BACKLOG.md` §待办2)。
   - → 10Gi 配额悄悄生效,无人知晓,直到出事。
2. **用户写入超 10Gi**:MDS 强制拒写 → `EDQUOT (errno 122)` → nuwax-file-server(Node)翻译成 `"Unknown system error -122"` 抛 HTTP 500 → 前端"请求没反应"。
3. **重设放宽 10Gi → 50Gi**:rcoder 再跑 `xattr::set(..., 50Gi)`,此时 caps/环境已变 → kernel client 直接 `denied`(代码注释 §737:连 `client.admin allow *` 都 denied);想确认当前值 → `getfattr` 读不回 → 不可观测;10Gi 配额仍在、EDQUOT 不消。
4. **止血**:跳过 rcoder,在挂载节点用 node admin 权限 `setfattr -x ceph.quota.max_bytes` 清掉所有 userId 旧 10Gi;代码 `:742-768` 整块注释禁用。配额功能完全关闭。

---

## 二、根因:三要素叠加(精确归因)

"10Gi 能设、50Gi 重设失败"**不是数值转换或重设逻辑的 bug**,是 kernel-mount + client-setfattr 这条路从根上不可靠。三个问题叠加:

1. **机制不可靠(行为漂移)**:kernel cephfs client 对 `setfattr ceph.quota.*` 虚拟 xattr 的放行与否,取决于 **caps × kernel 版本 × ceph 版本**组合;生产实测连 `client.admin allow *` 都 denied(`kubernetes_runtime.rs:737`)。"10Gi 设上"是旧 caps 下的偶然放行,"50Gi 设不上"是环境变了后的稳定拒绝——同一行代码,两副面孔。
2. **不可观测(致命)**:kernel client `getfattr` 读不回 `ceph.quota.*`;叠加代码的盲设逻辑,配额变成"薛定谔状态"——设成功没人知道(10Gi 悄悄生效),设失败 warn 退化为不限也没人知道,想 verify/重设又读不回。
3. **EDQUOT 已锁死**:用户写超 10Gi 后 MDS 已拒写,此时去"重设放宽"却设不上、看不到、改不动 → 死锁式困境,只能靠 node admin 强清 + 整体禁用止血。

> 最反直觉、最该记取的教训:**事故不是"配额没设上",而是"配额被意外设上了、真生效了、却完全不可见不可改"**——比 spec 完成标准担心的"静默退化为不限"更隐蔽,是反方向的盲区。

---

## 三、spec 现有表述的问题(待修正)

| spec 位置 | 现表述 | 问题 |
|---|---|---|
| §1.3 | "生产 kernel cephfs mount 不允许 set 虚拟 xattr(连 client.admin 都 denied)" | 归因基本对,但漏了"旧 caps allow rw 时曾意外设成功"这一关键事实——事故正是"设成功"引发,非"设失败" |
| §1.4 | "`ceph.quota.max_bytes` 是 advisory(soft);……CSI → hard 配额" | **不准确**:subvolume 的 `--size` 底层用的就是同一套 `ceph.quota.max_bytes` xattr,"soft vs hard" 的对立是错的。真实差别是"谁、在哪、用谁的凭据设配额" |
| §1.4 | "社区主流: per-PVC + CSI requests.storage → hard 配额 + 隔离" | "hard"易误导:CephFS 配额官方定性是 **cooperative + imprecise**,subvolume 也不例外;真正兜底的"墙"是 per-subvolume path-restricted 凭据(隔离),不是配额精度 |
| §8 | "CephFS subvolume 配额在生产 kernel client 的执行(需实测 EDQUOT)" | 对,但应补充:imprecise 容忍度取决于生产 MDS 统计延迟;cooperative 风险靠 subvolume 凭据隔离兜底 |

---

## 四、修改清单(执行指令)

> 修改目标文档:`specs/per-agent-storage-quota/per-agent-storage-quota-spec.md`

### 4.1 §1.3「已尝试方案:xattr 目录配额」——补全事故真相
- 保留"xattr 方案已禁用"的事实,但把失败归因补成:不是"设不上",而是 **"旧 caps 下意外设成功 → 用户写超 EDQUOT → 想重设却因 kernel client denied + getfattr 不可观测而无法止血"**。
- 明确失败根因三要素:① client 侧 setfattr 行为漂移(admin 都 denied)② getfattr 不可观测 ③ 代码盲设无 read-back 校验。
- 引用代码:`kubernetes_runtime.rs:736-740`(禁用注释)、`:742-768`(整块)、`:753-763`(set 无 read-back)。

### 4.2 §1.4「为什么 shared PVC + 子目录配额是社区 discourged」——修正 hard/soft 误述
建议改为(大意,措辞可润色):

> K8s 无原生 subPath 配额;client 侧 `setfattr ceph.quota.*` 在 kernel mount 下行为不可靠(依赖 caps × kernel × ceph 版本,生产实测连 `client.admin allow *` 都 denied)且 `getfattr` 不可观测(详见 §1.3 事故)。CephFS 目录配额的官方定性为 **cooperative(依赖客户端合作)+ imprecise(会短暂超限)**。社区主流方案 = **per-PVC(CephFS subvolume)+ CSI `requests.storage`**:配额由 ceph-csi 持 admin 凭据在**服务端**经 mgr volumes 模块设置(`ceph fs subvolume create --size`),绕开 client 侧 setfattr;subvolume size 在服务端可查(可观测);`subvolume resize` 服务端执行(可改、可扩容)。真正的租户隔离由 per-subvolume path-restricted cephx 凭据提供,而非配额精度本身。

### 4.3 §8「风险」——精确化 CephFS 配额风险
把"CephFS subvolume 配额在生产 kernel client 的执行(需实测 EDQUOT)"展开为两点:
1. **imprecise**:写入会短暂超配额(几十秒内),超量取决于 MDS 统计延迟 → 阶段 3 上线前需压测单 agent 持续写入的 EDQUOT 触发阈值与超限量。
2. **cooperative**:对抗性 client 理论可绕过 → 靠 per-subvolume 凭据隔离兜底(agent 只能 rw 自己 subvolume,越不过边界)。

### 4.4 §9「调研依据」——补充权威来源
- CephFS Quotas 官方文档(Limitations 章节):https://docs.ceph.com/en/latest/cephfs/quota/ —— 印证 cooperative + imprecise + path-based caps 限制。
- CephFS FS Volumes / Subvolumes 官方文档:https://docs.ceph.com/en/latest/cephfs/fs-volumes/ —— 印证 subvolume `--size` = 配额、mgr volumes 模块、CSI/manila 共用此接口。
- 代码佐证:`kubernetes_runtime.rs:740` 注释"后续配额方案:改用 ceph fs subvolume resize"——**当初代码作者已指向 subvolume 方向**。

---

## 五、方案选型权衡(供 spec 决策,建议写入 §1 或新增小节)

`docs/BACKLOG.md` §待办2 列了 3 个候选,本 spec §3.1/§7 选了**候选 1(per-agent PVC subvolume)**。差异对比:

| 候选 | 能否根治 EDQUOT 事故 | 租户隔离 | 改动量 | 备注 |
|---|---|---|---|---|
| 1. per-agent PVC subvolume(spec 当前方向) | ✅ 根治 | ✅ 强(凭据级) | 大(卷模型迁移 + file-server 重写) | 三合一:配额 + 隔离 + file-server 下沉 |
| 2. rcoder pod 改 ceph-fuse | ⚠️ 部分:userspace 允许 setfattr,但配额语义仍 cooperative/imprecise;需测 fuse 端 getfattr 能否读回 | ❌ 无(仍共享卷) | 小(chart 挂载方式 + SC mounter) | 若只想快速止血配额,代价最小 |
| 3. rcoder 持 admin key 服务端设目录配额 | ✅ 根治(绕开 client setfattr) | ❌ 无(仍共享卷 + 子目录) | 中(密钥管理 + mgr 命令封装) | 无 subvolume 隔离 |

**决策建议(修订时建议在 spec 中明确写出,避免读者误判目标)**:
- 若 spec 目标是**三合一重构**(配额 + 隔离 + file-server):维持候选 1,本修订只补强"为什么不是 2/3"的依据。
- 若目标是**先快速止血配额事故**:可拆一个 mini-spec 先做候选 2/3,per-agent PVC 按阶段推进(与 spec §6 增量路径一致——阶段 1 先 file-server 下沉仍共享 PVC,阶段 3 才切 per-agent PVC)。

---

## 六、执行约束(项目规范)

- **语言**:全部中文。
- **代码规范**:生产代码禁 `unwrap()`/`expect()`;异常加 `.context()`;遵守 Fail Fast + SOLID;Rust 禁 unsafe(FFI 除外)。
- **验证命令**(若改动涉及 Rust):`cargo check --features kubernetes --workspace` + `cargo clippy --features kubernetes --workspace`(K8s runtime 在 `#[cfg(feature = "kubernetes")]` 后)。
- **不要碰** `code/` 目录(build-agent-docker 侧同步产物)。
- 本修订**只改 spec 文档**,不改代码、不部署(部署由人工 helm 触发)。

---

## 七、参考依据汇总

- 事故记录:`docs/BACKLOG.md` §待办2(2026-07-15 EDQUOT 事故,含"旧 caps 意外设成功 / getfattr 不显示 / node admin 止血"全过程)
- 代码:`crates/docker_manager/src/runtime/kubernetes_runtime.rs:728-768`(配额块,已禁用;`:736-740` 为禁用注释,含"改用 ceph fs subvolume resize"的指路)
- 官方文档:
  - CephFS Quotas:https://docs.ceph.com/en/latest/cephfs/quota/
  - CephFS FS Volumes:https://docs.ceph.com/en/latest/cephfs/fs-volumes/
