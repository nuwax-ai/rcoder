# Scalar 文档 UI vendored 资产

`/api/docs/scalar` 页面的 UI 资产编译期内嵌于 rcoder 二进制（`include_bytes!`，
托管代码在 `src/router_docs/scalar_assets.rs`），浏览器离线/内网也可用——
上游默认模板从 jsdelivr CDN 加载 JS、从 fonts.scalar.com 加载字体，此处全部
改为同源路由。

| 文件 | 来源 |
|---|---|
| `scalar-api-reference.js` | `@scalar/api-reference` standalone 浏览器 bundle |
| `fonts/*.woff2` | `fonts.scalar.com` 的 Inter / JetBrains mono 字体分片 |
| `LICENSE` | 上游 MIT 许可证（Scalar） |

## vendor 时做的改写

1. bundle 头部注入一行 `/*! Scalar API Reference ... */` 溯源注释（版本、改写说明）。
2. bundle 内全部 14 处 `https://fonts.scalar.com/<name>.woff2` 改写为同源
   `/api/docs/scalar/assets/fonts/<name>.woff2`（`scalar_assets.rs` 的字体路由托管）。

除以上两处外内容与上游一致，未做其他修改。

## 升级步骤

```bash
cd crates/http-server/assets/scalar && ./vendor.sh <新版本号>
```

脚本完成后手动同步 `src/router_docs/scalar_assets.rs` 的
`SCALAR_ASSET_VERSION` 常量（bundle 路由 URL 含版本号，内容变 ⇒ URL 变），
并跑文档路由测试：

```bash
cargo nextest run -p http-server --no-fail-fast --all-features
```

测试守卫（`scalar_assets::tests`）会校验：模板与 bundle 零外链、bundle 字体
引用数与字体路由清单一致。
