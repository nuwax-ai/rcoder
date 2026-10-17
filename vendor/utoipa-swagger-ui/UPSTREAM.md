# 上游来源与兼容补丁

- 包：utoipa-swagger-ui 10.0.1，官方发布crate。
- 下载：https://crates.io/api/v1/crates/utoipa-swagger-ui/10.0.1/download
- 原始crate SHA256：3f5ec9d7816e4ce8ccb7e0bd7276e090cad0fcb1def4f3540e5b0f7485d7583e。
- 上游Git：https://github.com/juhaku/utoipa ，commit 4225d2ebefa86696affc69fdd5b38a8c38cb3eca，子目录utoipa-swagger-ui。
- 许可证：MIT OR Apache-2.0；两份原许可证保留。
- 上游窄兼容修改：规范化Cargo.toml的zip构建依赖8.6→9.0.5；build.rs的file.name()增加?，传播zip9新增的名称解码Result。公开API与全部src字节保留；Cargo.toml.orig记录原始清单。
- 未复制发布包中仅供上游自测、不会参与依赖解析的Cargo.lock，避免留下无效的旧锁；原始下载包checksum可重建全部14个原文件。
- 原因：RustSec RUSTSEC-2026-0367/0368/0369修复要求zip>=9；当前官方10.0.1及master仍使用^8.6。
- 主workspace与独立app-cli都必须通过各自顶层patch解析至本副本。移除条件：官方发布兼容zip9的版本并经同等构建/资源/归档测试后替换，删除此patch；不以忽略公告替代。
