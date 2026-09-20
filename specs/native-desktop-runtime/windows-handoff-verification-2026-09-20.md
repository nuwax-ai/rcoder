# Windows 当前源码普通宿主机流程验证

源码内容SHA256 `50a65a1e3873a061dbe0a3b6e1573bd22e2eb7e620878c82e63287c2e94bc510`；HEAD `432fe129f0cf0faf118cd9c8ddf6ff49513bbdbc` + 当时工作树，1070文件；归档SHA256 `e390795064343b085a1cdbf160479c3717ab4e7a692a238e108ac8fe7f7e96b7`。与本轮Mac源码内容相同。独立final-source目录、复用远端app-cli-target，未用本地target。

## 构建和环境

- app-cli 0.3.6原生cargo build退出0，1m02s；二进制SHA256 `12fff5457a2da044a357669ba9c906bfd83537917490a78ba19e05b14b7ed32b`。
- 可信私有Pingap0.14.3/cd74a461、Node、pnpm10.33.0、Python。业务运行PATH排除Git目录。
- 首次Git tar对Windows盘符解析失败，随后改用Python标准tarfile解包，正确构建通过；这属于测试准备错误。

## 真实普通业务流程

case `nativeowner3ec4f4e8e92b`，脚本exit0、passed=true。

1. 独立含空格工作区，从现有Vue模板源归档解包，真实app-cli build --dev成功，pnpm安装9.9s。
2. 首次Vite HTML可访问。
3. 修改HTML后重复serve，页面新标记可见，runtime_instance_id始终`e336a9a0-a00b-4f42-b0eb-921e66c0d85a`。
4. 显式控制协议Restart达到Succeeded，页面可见，同owner。
5. 显式Stop达到Succeeded，owner保持Idle/desiredStopped。
6. 再serve后页面恢复，同owner；第二次Stop成功。
7. Stop收束业务后，对测试自有owner执行Windows terminate清理，退出码1为TerminateProcess语义；**不据此宣称Ctrl+C优雅退出验证通过**。
8. 末尾按测试目录查无app-cli/node/Pingap/esbuild进程；3010/3018/9080/9081/5756无listener，核验exit0。

## 证据与未测项

- `/tmp/rcoder-native-windows-final-build.log`
- `/tmp/rcoder-native-windows-final-normal.log`
- `/tmp/rcoder-native-windows-final-cleanup.log`
- `/tmp/native_windows_final_normal.py`

本轮未执行proxy路径安全任务，不替代任何被拒绝任务；没有工具拒绝。未修改生产源码、仓库、配置、发布或提交。未测完整NT01–16、npm只读离线包、活动文件升级、PG凭据宿主机完整业务、Ctrl+C正常关闭。file-server-proxy本轮未重编或验收。普通Restart/Stop通过不代表这些未测项通过。
