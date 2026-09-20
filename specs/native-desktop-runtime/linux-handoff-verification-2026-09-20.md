# Linux 当前源码普通宿主机验收

源码内容SHA256 `50a65a1e3873a061dbe0a3b6e1573bd22e2eb7e620878c82e63287c2e94bc510`，与本轮Mac/Windows相同；HEAD `432fe129f0cf0faf118cd9c8ddf6ff49513bbdbc` + 当时工作树。root proxy构建另补tests-e2e工作区成员，补充内容SHA256 `989292355f0092aa98ba25f42c4e14a950b98a4759d7ccbe78e49e68fe8e53fb`。独立final-source目录，使用既有私有target/proxy-target。

## 构建

app-cli0.3.6原生构建exit0（17.81s），SHA256 `412ade05b5b5b252fd13bf26cb376adf552015839c73dd92d55c86c13aed6244`。proxy默认嵌入构建exit0（26s）；首次缺workspace成员构建失败已保留日志，不算产品缺陷。

## 普通核心链

`/tmp/rcoder-native-linux-final-normal.log` exit0，case nativeowner680a4f6e2f5c、passed=true。

- 真实Vue模板含空格独立路径，app-cli devbuild、Vite、Pingap0.14.3实际HTML。
- 修改源码后重复serve，同owner、新HTML可见。
- 显式控制Restart Succeeded、同owner/页面可见。
- Stop Succeeded，owner仍存活且desiredStopped；TERM owner exit0。
- 管理43365和3018/9080/5756释放；随后ss和本case进程查询无残留。

## 回执恢复

真实恢复链已完成，结果如下。首次立即衔接前轮时preflight bind撞TIME_WAIT；ss无listener/残留，测试改SO_REUSEADDR正常重跑。不删除资源或终止其他进程。

日志 `/tmp/rcoder-native-linux-final-build.log`、`/tmp/rcoder-native-linux-final-proxy-build.log`、`/tmp/rcoder-native-linux-final-normal.log`、`/tmp/rcoder-native-linux-final-recovery.log`、`/tmp/rcoder-native-linux-final-recovery-rerun.log`。

本轮只在个人机独立宿主目录测试，不操作K8s、PG、Docker或其他服务，不执行此前被拒绝的代理路径任务。未改生产源码、未本地Cargo、未提交。未验收完整NT01–16、npm只读离线包/升级/全鉴权和异常矩阵；不把当前核心通过扩大为全矩阵通过。

### 真实回执链最终PASS

case `recovery-real-2dd65e0795`，脚本exit0、passed=true。proxy SHA256 `c3646e241e09f18d27d8b038169805632f06d3a9e5551a6088497a0bd9f69dd0`。原操作真实Succeeded后正常退出owner；新instance读取原回执，recover新增POST0；新显式Restart task completed，POST总2，无额外Stop。

旧instance `66d66824-3aca-4193-af06-bdbac1990ce1` → 新instance `912f4657-813f-4e09-a140-e5a8ea8bf5fd`；原operation `fs-restart-79040e901da947c9a4403b7d404fec85` → 新operation `fs-restart-757a5ce27cdb410b8f6fefedc413f8e6`。不伪造终态、不修改生产持久状态。

最后独立bind确认3018/9080/9081/5756释放，按本轮两个case核查无app-cli/Pingap/node残留，检查exit0。
