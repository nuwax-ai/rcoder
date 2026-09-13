# 执行与证据（2026-09-13）

## 环境和入口

- SSH: `soddy@192.168.32.131`；节点为 192.168.32.131 / 192.168.32.226。
- namespace / Helm release: `nuwax-k8s-test`；chart `0.1.265`，Helm revision 65。
- rcoder 3 个 Ready 副本；主镜像、builder、app-runtime 均实录镜像身份；生产容器内 `app-cli 0.3.5`。
- 测试源码 HEAD `61998742f317388452f57d2bc883e4c1fa365367`。集群镜像身份独立记录，不仅凭源码 HEAD 推定镜像内 SHA。

```bash
make test-e2e-k8s-userapp \
  TEST_K8S_SSH=soddy@192.168.32.131 \
  RCODER_URL=http://192.168.32.131:30295 \
  E2E_PINGORA_URL=http://192.168.32.131:30435
```

最终 run: `d99b2dbc59b6440b8939738668e9e056`。

- 36 条实际断言：34 通过、2 失败。Python 返回 1，make 返回 2；不能判验收通过。
- 失败：`concurrent_ensure`、`concurrent_error_contract`。
- 通过：跨 3 个物理 rcoder 副本文件读写、重新 ensure、builder 单一身份且无残锁、A/B 真实构建与摘要、跨副本终态取消、SSE 回放与越界关流、冷部署、hot env 拒绝、SHA 失败保持 A、hot 切 B、部署身份、stop 幂等、零 Pod 后重新 start、全量定向清理。
- 热部署前后 Pod UID 相同；stop 后确认零 Pod；新 Pod UID `80f21800-940b-4281-8bbd-350817619aa5`，不同于旧 UID `4dc4f05e-ac0e-4c41-88ac-a83fb9c13f31`，内容仍为 B。
- 最终报告：[summary](../../tests-e2e/reports/d99b2dbc59b6440b8939738668e9e056/summary.json)、[断言](../../tests-e2e/reports/d99b2dbc59b6440b8939738668e9e056/assertions.json)、[全部轮次清理](../../tests-e2e/reports/d99b2dbc59b6440b8939738668e9e056/all-runs-cleanup.json)。报告目录被 gitignore，提交文档不会自动提交原始报告。

## 确认的问题

### K01 · P1 · 新建 PVC 的 claim 冲突直接终止 ensure

证据：`crates/docker_manager/src/runtime/k8s_builder_deletion.rs:71-90`，GET 读取 resourceVersion 后仅 PATCH 一次；409 直接上抛。

真实两副本并发请求中，一方 claim 遇到 PVC resourceVersion 变化返回 409，另一方因应用操作锁仍被占用而失败，两个请求均返回失败。随后显式顺序 ensure 成功，说明这次 builder 锁释放修复有效，但首次创建可用性仍有缺口。PVC 由控制器更新导致版本变化是符合代码和时序的解释，本轮未采集审计事件确定具体修改者。

建议：在 claim 本层对明确 409 重新读取，检查 UID 未换、归属一致且未终止，再以最新版本进行有截止时间的有限重试；UID 变化和传输不确定不能沿用旧资源身份继续重试。不要在所有 ensure 错误外围无条件重试。

### K02 · P1 · purge 把副本旧缓存视为新代写入，计算/存储已清理后仍遗留 prod 锁

首次 run `f073d5b6a87847e78970deeaca4e702c` 的 delete/app 返回：

```
ERR_CONFLICT
purge metadata cleanup not committed: cached application metadata changed during deletion
```

当时目标 workload/PVC 已消失，仅剩 `rcoder-operation-prod-e2e-k8s-f073d5b6a87847e7`。

代码证据：

- `crates/app_manager/src/runtime/metadata.rs:64` 每次登记生成新 generation。
- `crates/rcoder-storage/src/pg/userapp/repo/metadata_repo.rs:17-22` PG upsert 更新 generation。
- `crates/app_manager/src/runtime/metadata.rs:83-87` 删除快照读 PG；`:102-115` PG 条件删除后仍以本副本 cache 的 generation 判断冲突。
- `crates/app_manager/src/ops/storage.rs:408-420` 该错误发生在计算/存储删除之后，早退未到 release_lock.finish。

跨副本 cache 可以合法落后于 PG；这种差异本身不能证明删除期间发生新代写入。本轮已结束全部创建请求后再删除，观察结果与此路径吻合。

建议：让持久化层返回权威删除结果和代次；本地缓存提交/失效与本地更新串行，区分旧缓存与真正新代；设计已完成远程删除后的可恢复收尾，避免用永久操作锁保留单纯缓存冲突。不能简单取消所有归属检查或无条件释放不确定远程操作的锁。

首次清理失败保留为失败。在确认测试请求终止、计算和 PVC 已不存在后，仅对该 run 的锁执行了 UID/resourceVersion 条件删除；[独立恢复 receipt](../../tests-e2e/reports/f073d5b6a87847e78970deeaca4e702c/manual-owned-lease-cleanup.json) 记录真实操作。没有清理部署前已有锁。

### K03 · P2 · 冲突被统一包装为 ERR_CONTAINER_ERROR

证据：`crates/rcoder/src/userapp_forward/workspace.rs:74-85` 对 ensure 的所有错误统一映射 `ERR_CONTAINER_ERROR`；实际 PVC 409 和 operation-in-progress 均如此返回。

建议：保留结构化错误分类，将明确资源冲突映射稳定 `ERR_CONFLICT`，把未知传输/容器故障另行表达；同步其他消费方，不解析 message 文本猜测类别。

## 甄别和边界

- 未复现 builder 明确失败后永久占锁；真实 409 后重试成功是正向证据。真实 403、丢响应、5xx 注入未在共享集群执行，仍依赖此前组件/API 契约测试。
- release_id 请求与 manifest 身份不同的冷部署成功；协议 4 的操作身份及制品身份匹配。
- 第三轮 stop 的 ERR_VALIDATION 是新测试把 user_id 放 body 而非 query 导致，已修测试并在最终轮通过，不列为产品缺陷。
- 此轮实际链路为静态应用，无真实 AI、七语言工具链、数据库迁移、平台滚动升级或集群网络故障注入覆盖。
- 五轮独占应用的 Pod/STS/Deployment/PVC/Service/ConfigMap 全部确认无残留；第一次基线中的 workload/PVC UID 全部保留。并未据此声称验证了既有用户数据的每个字节。

## 测试入口本身的验证

`python3 -m unittest discover -s tests-e2e/tools -p test_k8s_userapp.py -v`：4 项通过，覆盖证据不含 Pod env、嵌套凭据脱敏、非致命失败不被后续成功覆盖、空场景不能通过。

`python3 -m py_compile tests-e2e/tools/k8s_userapp.py`、`git diff --check`：通过。

本轮仅添加测试入口、测试用例与文档，未修改 Rust 业务实现；尚未提交。
