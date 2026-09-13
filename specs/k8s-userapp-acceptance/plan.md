# K8s userApp 验收实现

使用 `tests-e2e/tools/k8s_userapp.py` 和 `make test-e2e-k8s-userapp`，与现有 Docker 套件独立选择。

1. 读取真实 Deployment、Pod、镜像 digest 和资源 UID，登记测试前基线。
2. 对 Ready rcoder Pod 分别建立 SSH 转发，避免 NodePort 随机分配无法证明跨副本。
3. 并发发起 workspace 创建，保留原始失败，随后显式调用 ensure 验证锁释放；失败项不因恢复成功而变绿。
4. 在各副本写文件并验证共享工作区；上传小型源码 ZIP，由真实 builder 构建 A/B，下载并核对 SHA256。
5. 验证跨副本任务终态取消、SSE 回放顺序及终态后越界游标自然关流。
6. 使用集群内 rcoder Service URL 下载真实制品，执行冷部署、hot env 拒绝、SHA 故障、hot 成功、stop/start。
7. 热部署核对 Pod UID/containerID/imageID；重启必须先等零 Pod，再核对新 UID 和 B 响应。
8. 保存唯一 run ID、源码/脚本指纹、实际镜像、操作身份、trace ID、断言和请求记录。凭据字段脱敏，不保存 Pod env。
9. API 定向删除前保存身份；删除后比对剩余资源和既有 workload/PVC UID。清理异常保持失败，人工恢复另存 receipt，不改写原结果。

只对状态暂态做有截止时间的轮询；业务失败立即记录，不无条件重试。并发阶段的失败允许继续执行后续独立验收步骤，但最终仍失败。
