# 聚焦真实入口验证

这三个脚本有强制前置和行为断言，未执行不代表通过。npm 协议 fixture 测试只证明包装层，不替代这里的 Rust／HTTP／容器结果。

## 二进制与源码回执

编译前保存源码指纹，源码不变时完成构建，再向同一 JSON 增加实际构建产物的 SHA256：

```sh
python3 tests-e2e/tools/userapp_root_logs.py --write-build-source /tmp/quality-build-source.json
# 此处构建待测二进制；Docker脚本必须使用与镜像架构匹配的Linux ELF。
```

回执格式沿用上述工具生成的 `source_inputs_sha256`、`commit` 等字段，增加：

```json
{"binaries":{"app-cli":"实际app-cli的SHA256","file-server-proxy":"实际proxy的SHA256"}}
```

不能在源码变化后只重新生成指纹来“匹配”旧产物；须重新构建。所有脚本检查当前源码、已登记哈希及实际文件；容器脚本还校验 ELF 架构、镜像身份和容器内复制后哈希。

## 入口

```sh
USERAPP_E2E_BUILD_SOURCE=/tmp/quality-build-source.json \
FILE_SERVER_PROXY_E2E_BINARY=/实际路径/file-server-proxy \
node crates/file-server-proxy/tests/e2e/native-link.test.js

USERAPP_E2E_BUILD_SOURCE=/tmp/quality-build-source.json \
node crates/file-server-proxy/tests/e2e/f4-volume-reuse.test.js \
  /实际Linux路径/app-cli /实际Linux路径/file-server-proxy dev-rcoder-agent-runner:latest

USERAPP_E2E_BUILD_SOURCE=/tmp/quality-build-source.json \
node crates/file-server-proxy/tests/e2e/f6-app11-fixture.test.js \
  /实际Linux路径/app-cli /实际Linux路径/file-server-proxy dev-rcoder-agent-runner:latest
```

- native-link：临时状态根，真实 npm→Rust→HTTP；仅确认捕获 owner 退出后删状态根。清理失败保留根并失败。
- F4：真正编译和 HTTP→强制回收本测试容器→原卷新容器重新编译／HTTP→Stop→Start。只验证本地运行链，不宣称 RCoder 闲置扫描器或 K8s 控制面通过。
- F6：合法、脱敏的 app11形态 Discovery/Generation；owner启动前导入，经只读平台绑定文件恢复、真实编译／HTTP、Stop/Start；旧未知运行结果不改成成功。它不是线上完整目录原件，也不能替代旧二进制升级对照。

容器脚本拒绝远端 Docker endpoint，不自动部署集群；仅清理带本测试唯一标签、捕获 ID 的计算容器，保留卷。报告位于 `tests-e2e/reports/`，可用 `USERAPP_E2E_REPORT` 指定路径。缺前置、任务失败、HTTP错误、源码漂移及清理未知均失败。
