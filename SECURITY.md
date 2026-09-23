# 安全策略

## 报告漏洞

如果你发现了安全漏洞，请**不要**开公开的 GitHub issue。

通过 [GitHub Security Advisories](https://github.com/nuwax-ai/rcoder/security/advisories/new) 私下报告，或联系维护者。请包含：

- 问题描述与影响范围
- 复现步骤 / PoC
- 受影响的版本

我们会在确认后尽快修复并发布安全公告。

## 支持范围

- `main` 分支与最新发布版本
- 更早版本按需评估

## 部署安全提示

- rcoder 控制面管理容器运行时（Docker socket / K8s ServiceAccount），请勿在不受信网络暴露
- deploy-host 形态默认只监听 `127.0.0.1` 并启用 api_key 鉴权，保持默认或自行加固
- 生产部署请启用 API Key 鉴权中间件并限制网络入口
