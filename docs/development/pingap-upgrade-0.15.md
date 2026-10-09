# Pingap 0.15.0 配套升级

业务运行时继续使用 Node 22.23.2。Pingap 的管理页面构建工具与业务 Node
独立；镜像使用官方预编译 full 制品，npm 平台包继续只提供 admin JSON API。

官方源码身份为 `v0.15.0 @ 8270a1ebb7a238ea86fa220215714613410378bb`。
`pingap-config`、app-cli 的运行身份默认值及镜像内二进制必须配套更新。
下载和缓存复用通过 `tools/build/pingap-assets.json` 核对官方资产身份及
SHA256，自算 manifest 的校验和不能替代官方校验和。

本地版本门禁：

```bash
python3 k8s/scripts/pingap_version_gate.py \
  --cross-repo --build-agent-docker /path/to/build-agent-docker \
  --node-version 22.23.2
```

## 构建仓使用准确源码

build-agent-docker 的默认版本选择只维护根 `versions.mk` 中的
`PINGAP_VERSION`，提交号由可信发行资产清单推导。RCoder 的 Make/Python
构建默认值直接读取本仓 app-cli 的 SDK 与运行身份契约，不重复写版本号。
升级新版本时仍需更新并验证 SDK pin、身份与官方资产清单；仅改镜像版本
字符串不能完成配套升级。

普通、direct 和 cluster 构建无需源码变量：优先读取相邻 `rcoder` 仓库中
与 `RCODER_BRANCH` 匹配的已提交 HEAD，否则在独立缓存中获取配置分支。
同一轮所有镜像共用一份冻结源码；预检及实际 Docker COPY 都核验它。
原有 `code/` 下的 clone 不会被 checkout、覆盖或写回，未提交的开发工作
也不会自动进入默认构建。

只检查默认输入，不启动镜像构建：

```bash
make runtime-preflight-all
```

需要构建某个精确提交时，可以显式选择：

```bash
make runtime-preflight-all \
  RCODER_SOURCE_REPO=/path/to/rcoder \
  RCODER_SOURCE_COMMIT=<完整升级提交>
```

仅传 `RCODER_SOURCE_REPO` 可选择该工作树内容。`make dev` 保留其原有
构建及测试仓推送流程；预检通过不能替代完整镜像构建或发布验收。

在每个架构上依次重建 app-runtime-base、app-runtime，以及安装 Pingap 的
agent-runner 末层。RCoder 主镜像使用同一源码提交。记录源码摘要、官方
tarball 摘要、实际二进制版本、基础与末层镜像身份，不能仅依据镜像环境
变量声明版本。runtime-only 会实际检查并固定基础镜像身份；旧 Pingap
基底或 Node 不匹配会在构建前被拒绝。

## 验证与交付边界

`tools/verify_pingap_upgrade.py` 用真实 app-cli 生成配置，使用可信清单对应的
官方 Linux 制品验证 admin、路由、错误来源头、WebSocket、SSE 和健康状态。
给出新旧 Linux app-cli/Pingap 后，可验证真实旧程序生成状态后的升级。
报告区分协议验证、升级生命周期、镜像及完整 Compose 门禁。

`tools/verify_userapp_subpath.py` 补充真实 React/Vue 的资源路径、Vite HMR
连接与更新事件。所有项目和改动留在临时副本，不修改模板仓库。

远端构建必须实际检查 computer/runtime 基础镜像的短版本、完整
commit/TLS 身份和 Node；部署版本元数据来自通过验证的构建回执。
旧回执缺少观察证据时需重新按构建流程取证。升级不能只改环境变量，
继续继承旧二进制。

0.15.0 尚未提供完整、权威的逐发布热载结果，版本升级不代表 P1 常驻代理
或热载确认问题已修复。回滚应恢复 app-cli/Pingap 成对制品及配置来源，
保留数据卷和业务状态。构建与测试不自动授权镜像推送、npm 发布或共享
环境部署。
