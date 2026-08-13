# rcoder 存储优化路线图

> 目的:系统性梳理 rcoder 在 K8s + CephFS 环境下的存储性能优化方向,供团队评估与排期。
> 依据:2026-08-12 生产 CephFS 性能诊断(实测)+ per-agent-storage-quota spec + 源码分析。
> 日期:2026-08-13

---

## 一、核心认知(决定优化方向的基础)

三个实测结论(来自生产只读诊断),决定了一切优化方向的边界:

### 1. per-file ~7ms 是 RADOS 物理成本,绕不过去

隔离实验铁证(诊断 4.8.6):同一批 200 个小文件——

| 拷法 | 耗时 | 说明 |
|------|------|------|
| 本地 emptyDir | **4ms** | 本地基线 |
| CephFS 散文件 cp | **1718ms**(8.6ms/文件) | 慢在"每文件一次数据对象写" |
| 同内容打 tar(更大)→ CephFS | **12ms** | 1 次对象写,快 100× |

→ **慢的不是字节数(带宽),是文件数(元数据 + per-file 对象写)。** 解法 = 减少往 CephFS 写小文件的次数(打 tar / 放本地)。

### 2. caps 争用只占 cp 慢的约 10%(纠正常见误解)

test 环境(几乎不抢 caps)cp 200 文件 ~1.55s,和 prod(~1.7s)几乎一样慢 → caps 争用不是主因。**per-agent PVC 消除 caps 巨头(~10%),但救不了单次 cp**(per-file 地板仍在)。

### 3. 社区共识:别把小文件树放共享 CephFS

Ceph 官方 + 诊断 P3:**node_modules、缓存、skills 这些可重建的小文件,本就该放本地**。CephFS 只扛"必须跨节点 RWX 共享"的少量数据。

---

## 二、数据分类原则(决定数据放哪)

优化设计的基石 —— 按"产生方和消费方是否同一个 Pod"分类:

| 数据 | 产生方 | 消费方 | 同 Pod? | 该放哪 |
|------|--------|--------|---------|--------|
| **skills** | file-server(rcoder Pod) | agent-runner Pod | ❌ 跨 Pod | CephFS 存 **tar 单文件** + 各 Pod 本地解压 |
| **用户源码/上传文件** | file-server | agent-runner | ❌ 跨 Pod | CephFS(共享) |
| **node_modules(web/custom-page)** | file-server(rcoder Pod) | vite(rcoder Pod) | ✅ 同 Pod | **本地** |
| **node_modules(computer/用户 install)** | agent-runner | agent-runner | ✅ 同 Pod | **本地** |
| **缓存**(pnpm store/npm/pip/uv) | 各 Pod 自己 install | 各 Pod 自己 | ✅ 同 Pod | **本地** |
| **PGDATA / 用户数据库** | agent-runner | agent-runner | 需持久 | CephFS(数据,不能本地) |

**关键区分**:跨 Pod 共享的(skills/源码)必须 CephFS(但可 tar 优化形态);同 Pod 闭环的(node_modules/缓存)走本地。

---

## 三、优化方向全景(按数据风险分层)

> ⚠️ **明确不做:清理 per-agent PVC**。这些 PVC 是**用户工作区数据**,per-agent spec 明确设计"agent 销毁后 PVC 保留、下次 ensure 重建挂回"(数据复用,`lazy_migrate` 也依赖它)。后面要启用 per-agent PVC 方案,清理它既**误删用户数据风险高**、又**与方案直接冲突**。nearfull 靠加盘(#1)+ 减少写入(#2/#7/#8)解决,**不靠清数据**。

### 🟢 零数据风险(缓存/配置类,丢了重建即可)

| # | 方向 | 解决什么 | 收益(实测) | 改动范围 | 工作量 |
|---|------|---------|-----------|---------|--------|
| 1 | **加盘 osd.3** | nearfull 88% | +4T 直接扩容 | 运维(集群侧) | 小 |
| 2 | **缓存目录全本地化**(`/cache` emptyDir) | pnpm/npm/pip/uv cache 写 CephFS | install 下载走本地(省 N 次写) | 镜像 ENV + Helm 挂载(rcoder + agent-runner) | 小 |
| 3 | **修 local-cache 配置 bug** | pnpm-store 注释说本地、实为 CephFS | 消除背景元数据噪音 | Helm(deployment.yaml) | 极小 |
| 4 | **skills import 临时目录本地化** | 解压 zip 写 CephFS | 2N→N(~0.5s→0.25s) | `skills.rs` 改 1 行 | 极小 |
| 5 | **去掉 PIP_NO_CACHE_DIR=1** | pip 每次全量重下载 | pip 启用本地缓存 | Dockerfile.base 删 1 行 | 极小 |

### 🟡 低风险(有回滚,rcoder 可控场景)

| # | 方向 | 解决什么 | 收益 | 改动范围 | 风险 |
|---|------|---------|------|---------|------|
| 7 | **custom-page build pod**(vite 下沉 WebAgentRunner + node_modules 本地) | custom-page vite/install 慢(**根治**) | node_modules 走 pod 内 emptyDir | rcoder + 镜像(不碰 java) | 中,详见 [设计](custom-page-build-pod-design.md) |
| 8 | **skills 彻底 tar 化** | skills 跨 Pod 共享 + 散文件 | N 写→1 写(~12ms) | file-server(打 tar) + agent-runner(解压本地) | 版本协调逻辑 |
| 9 | **per-agent PVC + cephfs-root** | 配额/隔离/caps 巨头 | caps 减负 ~10% + 防写爆 + 治理 | rcoder(代码就绪) + Helm | 数据迁移(有 lazy_migrate + 回滚) |

### 🟠 需谨慎(改数据流/架构,收益大但要充分验证)

| # | 方向 | 解决什么 | 收益 | 改动范围 | 风险 |
|---|------|---------|------|---------|------|
| 10 | **agent-runner node_modules 本地化**(B1 软链) | computer 场景用户 install 慢 | node_modules 写本地 | agent-runner 启动逻辑 | 工具链兼容(pnpm/npm/node/python)+ 动态 install 覆盖 |
| 11 | **扩 MDS 1→2** ⏸️暂不做(仅记录) | 元数据吞吐/test 排队 | 改善整体吞吐 | rook filesystem.yaml(运维侧) | 维护窗,**不救单次 cp**;暂不做 |

### 🔵 可靠性(非性能,但重要)

| # | 方向 | 解决什么 | 收益 | 风险 |
|---|------|---------|------|------|
| 12 | **min_size 1→2** | primary 宕机丢近期写 | 数据不丢 | 每写多等 1 副本 ack(延迟略增) |

---

## 四、重点方向详解

### #2 缓存目录全本地化(``/cache`` emptyDir)—— 性价比之王

**现状**:agent-runner 的 workspace 挂 `/home/user`(CephFS),而 `PNPM_HOME=/home/user/.local/share/pnpm`、npm/pip/uv cache 默认在 `/home/user/.xxx` → **缓存全在 CephFS 上**,install 时小文件风暴。且 `PIP_NO_CACHE_DIR=1` 禁用了 pip 缓存(每次全量重下载)。

**方案**:约定 `/cache` 目录(emptyDir),镜像层配环境变量让缓存都走那:
```dockerfile
ENV XDG_CACHE_HOME=/cache \
    npm_config_cache=/cache/npm \
    PNPM_HOME=/cache/pnpm \
    npm_config_store_dir=/cache/pnpm/store \
    PIP_CACHE_DIR=/cache/pip \
    UV_CACHE_DIR=/cache/uv
    # 删掉 PIP_NO_CACHE_DIR=1
```
Helm 层:rcoder 主 Pod + agent-runner 都挂 `/cache` → emptyDir(sizeLimit 10Gi)。权限用 fsGroup 或启动 chmod。

**收益**:install 下载缓存走本地(不写 CephFS)。零数据风险(缓存可重建)。
**残留**:node_modules(产物)仍在项目目录(CephFS),但 pnpm store 本地后用 symlink materialize(条目写,比数据写少)。

### #7 custom-page build pod:vite 下沉 WebAgentRunner + node_modules 本地化(根治)

> 完整设计见 [custom-page-build-pod-design.md](custom-page-build-pod-design.md)。

**最终方案**:把 vite/build 从 rcoder 主 Pod 下沉到 **WebAgentRunner pod**(单 pod:agent 对话 + 内嵌 file-server 一起),node_modules 走 pod 内 emptyDir(本地),源码留 CephFS(共享,HMR 正常)。

**核心简化**:file-server 整体下沉到 pod → dev server 状态跟着走,**不用分布式重构**(化解了"跨 Pod 状态"的最大风险)。复用 WebAgentRunner ServiceType(不新增枚举、不碰 java)。

**满足前提**:file-server 文件服务(源码 tree/git/下载)留控制面读 CephFS;build 接口自动 ensure WebAgentRunner(没起就拉起);对外 HTTP 接口不变。

**改动**:7 项,集中在 `handlers/build.rs`(转发+ensure)+ Helm(emptyDir)+ port_proxy(端口路由)+ 镜像(内嵌 file-server)。全在 rcoder + 镜像范围,不扩散 java。

**风险/.20 验证**:WebAgentRunner 内嵌 file-server 跑 dev server + HMR、node_modules 软链的 pnpm/esbuild 兼容、port_proxy 端口路由。有回滚开关(env 切回控制面 spawn vite)。

### #8 skills 彻底 tar 化(跨 Pod 共享优化)

**现状**:`sync_agents` 已用软链(fan-out 优化,✅)。剩余慢点:`import_skill_archive` 解压后 `copy_entry` 逐文件写 `.agents/skills`(CephFS)。

**方案**(file-server + agent-runner 都改):
- file-server:接收 skills → 打 tar 单文件 → 写 CephFS(1 次写,~12ms)
- agent-runner:启动检查版本 → 读 tar → 解压到本地 emptyDir → `.claude/skills` 软链本地

**收益**:skills 从 N 写 → 1 写,跨 Pod 共享靠 CephFS(tar)、消费在本地。
**关键**:file-server 先写、agent-runner 后启动也能拿到(tar 持久在 CephFS)。

### #9 per-agent PVC + cephfs-root(配额/隔离/治理)

**定位**:配额(防单 agent 写爆,EDQUOT 事故教训)+ caps 减负(~10%)+ 隔离/精细清理。**不救 cp 慢**(per-file 地板)。
**状态**:代码大部分就绪(`RCODER_PER_AGENT_PVC_ENABLED` + `SubvolumeWorkspaceResolver` + `resolve_subvolume_path` + `lazy_migrate`),缺口=未在真实 ceph 验证。
**详见**:`specs/per-agent-storage-quota/`。

### #10 agent-runner node_modules 本地化(computer 场景根治)

**现状**:用户在 agent-runner 沙箱 pnpm/pip install,node_modules 写 `/home/user/project/node_modules`(CephFS)。这是 computer 场景 install 慢的根因。

**方案**(B1 软链):agent-runner 启动时扫描 workspace 的 node_modules,数据迁到本地 emptyDir + 建软链 `proj/node_modules → /local-deps/xxx`;配合 #2(pnpm store 本地),用户新 install 也走本地。
**风险**:工具链对 symlink node_modules 兼容(pnpm/npm/node/python)+ 用户动态 install 新目录的覆盖。
**这是唯一能根治 computer 场景 node_modules 慢的方向,但复杂度最高。**

---

## 五、推荐推进路线图

```
第一阶段「立即/本周」(零风险,高收益):
  #1 加盘(今晚) + #2 缓存本地化 + #3 修配置bug + #4 skills import + #5 pip缓存
  → nearfull 缓解 + install 下载走本地 + 配置理顺

第二阶段「.20 验证」(低风险,rcoder 可控):
  #7 rcoder node_modules 本地化 + #8 skills tar 化
  → custom-page vite 彻底快 + skills 跨Pod优化

第三阶段「.20 验证」(治理):
  #9 per-agent PVC(配额/隔离/防写爆)

第四阶段「啃硬骨头」(高收益高复杂度):
  #10 agent-runner node_modules 本地化(computer 场景根治)

观察/可选:
  #11 扩MDS + #12 min_size
```

---

## 六、诚实的期望管理

| 痛点 | 能解决到什么程度 |
|------|----------------|
| nearfull 88% | ✅ #1 加盘直接解决 |
| custom-page vite install 慢 | ✅ #7(rcoder 可控)根治 |
| skills 推送慢 | ✅ 软链已解决大头 + #4/#8 补残余 |
| **computer 场景用户 node_modules 慢** | ⚠️ #2 缓存本地缓解下载;#10 才根治(复杂);剩余是 CephFS per-file 本征成本 |
| 配置 bug(pnpm-store 在 CephFS) | ✅ #3/#2 马上修 |

**核心策略**:rcoder 能控的(#2/#7/#8)尽量本地化;用户行为产生的 node_modules 靠 #2(缓存)+ #10(软链)+ #1(加盘)缓解;per-agent PVC(#9)管配额防写爆。

---

## 七、相关文档

- `specs/per-agent-storage-quota/` —— per-agent PVC + cephfs-root 详细 spec(#9 的依据)
- `specs/per-agent-storage-quota/cephfs-perf-implications.md` —— CephFS 性能实测对 per-agent 方案的影响(核心认知来源)
- `specs/per-agent-storage-quota/spec-revision-brief.md` —— 2026-07-15 EDQUOT 配额事故根因
- 生产 CephFS 性能诊断完整实测数据:在 build-agent-docker 仓库 `k8s/docs/developer-guide/cephfs-performance-diagnosis.md`(独立项目,本文只提炼结论)
- 节点维护流程:`build-agent-docker/k8s/docs/developer-guide/node-maintenance-guide-ha.md`

---

## 八、验证环境

- **开发测试集群**:`192.168.1.20`(单节点 k3s + 真实 Rook-Ceph,cephfs myfs 就绪,rcoder 已部署,容量空闲)。#7/#8/#9/#10 都在此验证。
- **生产**:nuwax-k8s-prod/test(三节点 etcd HA + Rook-Ceph)。cutover 走维护窗 + 回滚开关。
