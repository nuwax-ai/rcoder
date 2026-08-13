# CephFS 性能实测对本方案的影响

> 依据:2026-08-12 生产集群只读性能诊断(完整实测数据见 build-agent-docker 仓库 `k8s/docs/developer-guide/cephfs-performance-diagnosis.md`;两个仓库独立,本文只提炼对本 spec 方向有影响的结论,不交叉引用)。
> 日期:2026-08-13

---

## 1. 一个必须纠正的认知:caps 争用只占 cp 慢的约 10%

本 spec 原本隐含的假设:"共享 PVC 导致 caps 争用 → per-agent 独立 subvolume 消除争用 → 提速"。

**实测推翻了"caps 争用是慢的主因"** —— 诊断 4.8.4 的 test 对照实验:

| 环境 | caps 持有 | cp 200 小文件耗时 |
|------|----------|-----------------|
| prod(112 万 caps,争用激烈) | 96% | ~1.7s |
| test(47k caps,无 runner、几乎不抢) | 4% | ~1.55s |

test 几乎和 prod 一样慢 → **caps 争用不是慢的主因**。

cp 慢的真正结构地板是 **per-file 数据对象写(~7ms/文件)**(诊断 4.8.6 隔离实验:`touch` 200 空文件 40ms,带数据 `cp` 才 1400ms)。这是 RADOS 写对象的本征成本,与有没有人抢 caps 无关,与 MDS rank 数无关。

## 2. 对 per-agent 方案定位的影响

| per-agent PVC 能解决的 | per-agent PVC 不能解决的 |
|------------------------|------------------------|
| 配额(防单 agent 写爆,本 spec 原始动机) ✅ | cp skills / node_modules 慢(per-file ~7ms 地板仍在) ❌ |
| caps 减负(消除 99% caps 巨头,约 10% 放大) ✅ | |
| 隔离 / 精细清理 / 治理 ✅ | |

> **结论:per-agent PVC 仍然值得做(配额 / 隔离 / 治理),但"性能 / 提速"不是它的卖点。** 若业务痛点是"cp skills 慢",per-agent PVC 救不了,需要的是"小文件本地化 + skills 打 tar"(见第 4 节)。

## 3. 一个利好:cephfs-root 聚合挂载实测安全

诊断 4.3 实测:rcoder 挂整个 fs 根的 `cephfs-root`(rootPath=/)**只持 4 个 caps,基本闲置**。

这消除了 spec §4 的潜在担忧("rcoder 挂根聚合会不会成为新的 caps 巨头")—— 实测证明不会。**阶段 2"rcoder 静态 PV 挂根聚合访问所有 subvolume"是安全的,不增加 caps 压力**。真正的 caps 巨头是被多 pod 共享的 computer-workspace(99%),不是挂根聚合。

## 4. 真正解决 cp 慢的方向(与本 spec 并行,需协调)

社区共识(Ceph 官方 + 诊断 P3):**per-file ~7ms 是 RADOS 物理成本,没有 CephFS 调优能绕过。解法是"小文件不放 CephFS"**:

| 文件类型 | 应放位置 | 对应改造 | 收益(实测) |
|---------|---------|---------|-----------|
| node_modules / pnpm-store / build cache | 本地 emptyDir | P1a(改 Helm volume + env) | 本地 4ms vs CephFS 1718ms |
| skills(散文件 cp) | tar 归档 + 解压到本地 | P1b(file-server 改造) | 200 次写 → 1 次,~100× |
| 项目源码(真正需 RWX 共享) | CephFS(保留) | - | - |

> pnpm-store 当前配置 bug(诊断 4.5):deployment.yaml 注释说"放本地避免竞争",实际挂在 CephFS(templateCache.storageClass=cephfs)。这是最容易修的一处。

## 5. 优先级修正(综合诊断)

```
P0  容量(加盘 / 清可重建数据)          ← nearfull 88%,到 95% 集群只读,最紧急
P1  小文件本地化 + skills tar          ← 救 cp 慢的主线(per-file 地板)
P2  per-agent PVC(本 spec)             ← 配额 / 隔离 / caps 减负(~10%)/ 治理
P3  扩 MDS(1→2 + 给资源)              ← 改善整体元数据吞吐,不救单次 cp
```

**本 spec(per-agent PVC)= P2**,定位明确为"配额 / 隔离 / 治理",**不是"性能优化"**。

## 6. 对 spec 现有结论的补充(不改方案方向)

- **§2 目标**:per-agent 配额 + 保留"不启动 agent 也提供服务" + 无缝迁移 —— 不变,仍是本 spec 价值。
- **§3.1 架构**:数据隔离面 + 管理聚合面 —— 不变。补充依据:管理聚合面(rcoder 挂根)实测只持 4 caps,安全(第 3 节)。
- **§7 风险**:补充一条 —— "per-agent PVC 不解决 cp 小文件慢(per-file ~7ms 地板),若业务同时有性能诉求,需并行做小文件本地化 + skills tar(P1),不能只靠 per-agent"。
- **新增认知**:本 spec 与"P1 小文件本地化 + skills tar"是**互补关系,不替代**。per-agent 管"配额/隔离",P1 管"性能"。两者可并行推进,数据迁移(cutover)时注意先后(先 P1 减负载,再 per-agent 切换更稳)。
