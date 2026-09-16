# 远端部署与基础测试复验

日期：2026-09-16。

## 结论

现有 `make/remote-k8s.mk` 与 Python 工作流已支持 IP + NodePort，无需为直连测试修改逻辑。本轮没有修改工作流或业务代码，保留并行开发改动。

## 实际执行

| 命令 | 退出码 | 结果 |
|---|---|---|
| `python3 -m unittest discover -s tools/remote_k8s/tests -v` | 0 | 11 项通过 |
| `make remote-k8s-verify SUITE=smoke` | 0 | 同步、快照、amd64 构建、推送、部署及 smoke 通过 |
| `make remote-k8s-test SUITE=gateway` | 2（Make） | Gateway HTTP 请求超时，失败 |

额外从 Mac 连续请求直连 NodePort `/health` 五次，全部 HTTP 200、业务 code `0000`，耗时分别 13、10、3、5、5 ms。

两个 RCoder Pod 分别运行在两个节点上，均 1/1 Ready、0 次重启。smoke 检查了部署身份、双副本就绪、四个 PVC Bound 和直连健康接口。未运行本轮 UserApp/Chat 完整业务套件，不以健康检查代替业务验收。

## 部署身份

- build_id：`20260916T025927Z-853f6480`
- source_sha256：`0078e9ea2ad562966368a1fd4f11b478e59a8eb369707dff3e3ec29d89637e92`
- rcoder：`sha256:a15087f65afc817f33b6bbce5eb3a86443bee1807e775b9392472f7029569177`
- computer：`sha256:fed42f883517988f4d760a5389890dc524dbbbb24e5ac508173f58a0aebb4686`
- runtime：`sha256:e462c2c1ba7064f2871fc4ab12df73f60133ad3989248e4688af831dc4a25e74`
- smoke report ID：`3968fcd1d3c34097b611b72ab2c87d01`
- gateway report ID：`d3c99207c6724cb987c6bcb4b07a9600`

报告位于 `.remote-k8s/<环境ID>/tests/`；真实入口读取 `.env.local` 和 `deployment.json`。部署保持运行。本次证明的是上述固定源码快照，不代表其后工作树修改已经部署。

## 后续使用

```bash
# 当前源码重新构建、部署和基础测试
make remote-k8s-verify SUITE=smoke
# 仅复测已部署版本
make remote-k8s-test SUITE=smoke
# Gateway 修复后单独验收
make remote-k8s-test SUITE=gateway
```

继续保留 Gateway 的真实失败，不将 Gateway 套件改为直连检查。集群补全建议见 `cluster-gap-analysis-2026-09-16.md`。
