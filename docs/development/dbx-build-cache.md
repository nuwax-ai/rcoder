# DBX 镜像构建缓存

`make build-dbx-fork` 在本仓自行获取源码和构建，不要求检出生产构建仓。

每次调用先获取指定 ref，捕获完整 Git SHA，再从 Git archive 生成独立源码快照。
基础镜像先解析 digest，快照 Dockerfile 使用 digest；pnpm 使用源码 `packageManager` 的版本。
缓存身份覆盖源码 SHA、来源、双架构、构建脚本、Dockerfile、忽略规则、基础镜像 digest、构建平台、builder 实际配置和显式 pip 构建参数。
manifest 中的凭据脱敏，完整参数仅以摘要参与身份。

持久目录默认为仓库父目录的 `.cache/nuwax-build/dbx/v2/`，可用配置调整，路径按实际 Make 文件确定：

- `sources/`：可重建的 bare 源码镜像。
- `locks/`：稳定内核文件锁；不删除或替换锁文件。
- `entries/<输入摘要>-<代次>/`：不可变二进制、静态前端和 `manifest.json`。
- `refs/<输入摘要>.json`：已验证缓存代次的查找指针。

查找缓存时核验全部资产摘要、ELF 架构、可执行权限和 fork 特征；损坏内容重新构建。
旧 `stage/`、`fork.stamp` 和历史 generation 保留，不为 v2 提供有效身份。
`FORCE_DBX_FORK=1` 创建新 generation，不覆盖旧 generation。
构建使用唯一 tag，并按 BuildKit `--iidfile` 返回的 image ID 提取资产。
分发先复制和校验所有目标，再发表；失败传播到 Make，保留已有有效资产。

构建调用者可以传入 `DBX_OUTPUT_REF=<本次调用独有的文件>`，成功后获取 JSON：

```json
{"protocol":2,"entry":"/absolute/cache/v2/entries/<key>-<generation>","key":"<输入摘要>","manifest_sha256":"<manifest 摘要>"}
```

Docker 的单次构建快照应从该不可变 entry 读取，先验证 manifest 摘要及 `files`，再复制。
固定 `downloads/` 仅用于既有入口分发，不应作为并发镜像构建的资产身份。
显式 `download-dbx-cache` 使用官方镜像 digest，与 fork 使用不同缓存身份。

本地协议回归不调用真实 Docker，也不执行交叉编译：

```bash
python3 -m unittest tools.tests.test_dbx_cache
```

真实双架构构建、动态下载的系统依赖及容器 HTTP 自检需要另行执行镜像构建验证。
协议测试不能代替上述运行时验收。

DBX 交叉编译工具的精确版本由本仓 `make/dbx-versions.mk` 维护，并传入缓存 helper。快照中的 ziglang 使用 `==` 精确版本，cargo-zigbuild 使用 `--version` 和 `--locked`，工具版本写入输入身份。改变任一版本会创建不同缓存 key；未识别的安装命令或非精确版本在构建前失败。

Docker context 的临时快照固定创建在脚本所属仓库 `.cache/build-contexts/`。宿主缓存路径不能证明 Docker/BuildKit 编译层的实际存储位置，应分别核验。
