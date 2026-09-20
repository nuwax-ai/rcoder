# 当前源码 macOS 原生验证与三平台剩余矩阵

## 来源

源码内容 SHA256 `50a65a1e3873a061dbe0a3b6e1573bd22e2eb7e620878c82e63287c2e94bc510`，归档 SHA256 `c83d762f490cb42b3068daf0fb22686d60412821267b90f315a79500c56a9885`，1070 文件；基线 HEAD `432fe129f0cf0faf118cd9c8ddf6ff49513bbdbc` + 当时工作树。远端独立源码快照；复用私有 target，不运行本地 Cargo，不影响 Compose。

app-cli 编译 exit 0（13.12s）；proxy 默认 embed-file-server 构建 exit 0（15.21s）。首次误用不存在的 feature `embed` 被 Cargo 拒绝；随后正确构建完成，不隐去失败。日志 `/tmp/rcoder-native-final-build.log`、`/tmp/rcoder-native-final-proxy-build.log`。

## 本轮通过

- 真 Vue 模板 devbuild/devrun；纯前端无需数据库；重复 serve 后 HTML 新标记可访问、owner instance 不变。
- Stop 终态成功、owner 留在 Idle；TERM owner exit0；管理端口和3018/9080/5756全部释放。
- case `nativeownere5ef1ff7c9c3`，app-cli SHA `4c2c98a9427422c715aaab90ce8c517bd55fb2c9d06c5ef1dae875cd383ce08e`；日志 `/tmp/rcoder-native-final-source-smoke.log`。
- 未知 HTTP 服务占3010并伪造health200：app-cli19ms退出1，workspace只有原sentinel未改变；受控监听自行退出。日志 `/tmp/rcoder-native-final-conflict.log`。

回执恢复/显式Restart及中文空格路径均已完成，结果见后文。

## 三平台验收边界

历史证据散布 `/tmp/rcoder-native-macos-followup.md`、`/tmp/rcoder-native-linux-followup.md`、`/tmp/rcoder-native-windows-followup.md`、`/tmp/rcoder-native-windows-proxy-followup.md`、`/tmp/rcoder-native-artifact-target-followup.md`。Linux/Windows历史源码不同，不能冒充本轮快照通过。

- NT01/03/05/08：三平台有核心前端/owner/占用/清理证据；不等于并发和超时全部组合完成。
- NT02/06/07：proxy文件与路径及保护已有局部证据，仍需全三平台逐项认证/坏状态/跨进程竞争清单。
- NT04/10：各平台有特殊路径和身份局部证据；双项目业务端口独立和全部junction/精简PATH组合未完整验收。
- NT09/12：macOS真实丢响应+owner重启恢复与任务链有证据；三平台全部取消/SSE/崩溃矩阵未完整执行。
- NT11：Linux真实PG就绪/URI探测通过；凭据轮换完整业务由Compose本轮门禁验证，不作为三平台原生全覆盖。
- NT13/14/15/16：完整离线只读包、跨目标/ABI拒绝、活动文件升级、TS兼容模式仍缺完整三平台验收。源码/组件/包装静态测试不能替代实际包验证。

本轮仅验证源码构建产物，未发布npm、镜像或Git提交。

## 本轮真实恢复链通过

`/tmp/rcoder-native-final-recovery.log` exit0，case recovery-real-b984ed0585、passed=true。真实旧operation Succeeded后owner退出并生成新instance；file-server recover只GET终态，新增POST=0；后续显式Restart新operation、task completed，总POST2无预Stop。

proxy SHA256 `0e1358ce6eddb037149cf048bff8866aebf6860743c3e88f9eb65c0f367baf2a`；app-cli与上文相同。旧instance f39909d3-6818-46a9-99bb-53b34105a34f → 新instance c8044588-7471-4176-b467-7a96bbd44b0b。原operation fs-restart-75ebd43f955b4e47bd4ba081b0a81ec1，新operation fs-restart-f41724e2d7a54dccbfc1f61a2bafee56。真实代理仅注入一次丢响应，不伪造owner终态。

## 中文空格路径与最终资源清理

`/tmp/rcoder-native-final-path.log` exit0、passed=true，case nativeowner9f35705d53e9。真实工作区 `中文 project.with spaces`：devbuild完成、Vite HTML、重复serve同owner且页面更新、Stop保留Idle、TERM exit0；不是helper静态测试。instance ded522e2-b0e3-4526-9c99-0a54082e6042。

所有测试结束后，额外真实bind核验3010/3018/9080/9081/5756可用；按三例路径核查无app-cli/Pingap/node残留进程，检查exit0。未清理其他任务资源。当前工作区未修改生产或测试源码；报告仅在/tmp。
