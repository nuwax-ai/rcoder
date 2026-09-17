# 三平台原生运行时：app-cli 与 file-server-proxy

日期：2026-09-17。性质：用户新增需求及实施交接，**尚未实现或验收**。

## 结论

两者可以在 Windows、macOS、Linux 宿主机运行，并随 Electron 客户端分发。推荐沿用原生 Rust 进程及现有 HTTP/SSE 契约，不引入 Docker、K8s、supervisord 或远端控制面作为桌面前置。

当前源码具备跨平台构建、builtin 编排和内嵌 file-server 的基础，但仍有数据库等待、容器路径、固定端口、全局 PID 状态及打包依赖等缺口。不能将“发布矩阵包含 Windows/macOS”当作原生完整功能已交付。

“自洽”的目标是安装产品包即可使用基础能力，产品自己携带和管理必要组件。保留少量私有辅助进程可以达到这一目标；不要求用户另装常驻服务，也不为了压成一个可执行文件重写现有代理协议。用户项目自己的 Node/Java/数据库依赖单独声明、校验和提供配置。

## 阅读顺序

1. [spec.md](spec.md)：范围、依赖边界和用户可见行为。
2. [plan.md](plan.md)：当前源码问题 N01–N10、技术方案及取舍。
3. [tasks.md](tasks.md)：分批实施、三平台真实测试和交付门禁。
4. [verification.md](verification.md)：本轮实际证据及后续记录模板。
5. [统一开发提示词](../development-review-2026-09-17/claude-prompt.md)：连同此前 RCoder 和配套镜像问题一起处理。

## 与已有方案的关系

- 继承 [运行态所有权](../userapp-runtime-ownership/spec.md)及[跨平台约束](../userapp-runtime-ownership/cross-platform.md)，补充无容器宿主机、file-server-proxy 和 Electron 的要求。
- [RCoder 审查 R01–R11](../development-review-2026-09-17/review.md)与[镜像审查 B01–B05](../development-review-2026-09-17/build-agent-docker-review.md)继续有效；本方案不替代它们。
- 原生验收、Compose、remote-k8s、npm 发布和 Electron 成品安装验收分别记录。
- 本目录不包含个人测试机器地址、账号或密码。访问参数只存放于不提交的本地配置。
- Electron 仅是后续使用场景。本轮聚焦两个组件自身，不开发 Electron 主进程/renderer、IPC SDK、客户端安装包或自动更新；仅明确外部程序调用所需的路径、启动和生命周期契约。
