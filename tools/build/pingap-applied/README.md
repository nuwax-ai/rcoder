# RCoder 配对 Pingap

此目录保存官方严格 pin 的 reviewed patch；不是官方 release 资产。`tools/build/pingap-assets.json`
继续保存未经修改的官方资产与 SHA256，本目录不会覆盖它。

`apply.py` 在干净 checkout 上核验 exact base commit、版本、patch SHA256，执行 `git apply --check`
后应用包含新增生产模块、测试、fixtures 和双语文档的 patch，再核验全部改动文件 SHA256。
重复验证使用 `--verify-only`，不会重置已有源码。`--repo-root` 另外核对 app-cli 的官方 source pin。

```bash
python3 tools/build/pingap-applied/apply.py --source ./pingap-source --repo-root .
python3 tools/build/pingap-applied/build.py --source ./pingap-source --already-applied \
  --repo-root . --tls rustls --output ./bin/pingap
# 隔离 fetch + patch + 原生 release 构建：
python3 tools/build/pingap-applied/build.py --fetch --source ./pingap-source \
  --repo-root . --full --output ./bin/pingap
```

构建使用 locked 依赖；产物旁的 `.applied.json` 明确记录官方 base、RCoder patch SHA256、
实际 binary SHA256、Rust 工具链、target、features 和是否实际执行了原生 capability 查询。
`--apply-protocol-version` 必须输出 `1`；旧二进制应在停业务之前拒绝。跨 target 构建不伪装原生验证，
由 CI 的目标平台 pair gate 或真实部署验收执行二进制。目录内 manifest 不保存本机绝对路径。

默认及 full feature 测试、strict Clippy 在 release workflow 的专用 gate 中执行；release build 及
npm/容器发布仍是各自独立结果。Docker 最终层必须覆盖本目录构建的 binary 并核验 capability。
