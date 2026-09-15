# 接入任务与验证证据

状态：仅完成研究与规划。以下未勾选项均未实施或验证。

## 已完成的文档工作

- [x] 核对锁定版本依赖与 watcher/await_condition/Recorder 源码。
- [x] 分离调研、行为规范、实现方案、验收任务。
- [x] 克隆官方 kube-rs 4.2.0 源码，核对 tag/commit、干净工作树，以及三个关键源文件与 registry 的字节一致性。
- [x] 记录前一轮集群只读证据及局限；没有将其算作新实现验收。

## 执行任务

- [ ] T01：确认并行生命周期变更落点，固定 HEAD＋相关 diff 指纹、现有聚焦测试基线。
- [ ] T02：补 Pod 纯分类器回归；在旧实现上暴露相关等待预算/错误恢复缺口，不修改既有业务成功定义。
- [ ] T03：建立有总超时的流式 HTTP 契约测试，先证明测试可检测取消泄漏、410 后不恢复、403 重试错误。
- [ ] T04：实施私有有界观察器、Pod Ready 迁移；验证默认/K8s features，独立提交。
- [ ] T05：补 STS/Pod 替换、旧 Pod Ready、replicas 变化的确定性回归，再迁移 builder 控制观察。
- [ ] T06：补 Event 队列/发布失败/关停测试，再接入 publisher 和三个配置来源的 RBAC。
- [ ] T07：核对严格 E2E 必测登记，增加本方案断言与指标证据；保留现有 userapp/chat 场景。
- [ ] T08：一次性执行合并后的格式、默认/K8s Clippy、相关与 workspace 测试，记录全部退出码。
- [ ] T09：冻结源码，检查远端客户端版本与权限，执行 userapp 和真实 chat 验收。
- [ ] T10：固定基线与候选镜像采集 A/B，记录原始样本；无数据则性能收益仍未验证。
- [ ] T11：核对基线资源保留、测试资源清理和残留操作，形成最终交付报告。
- [ ] T12，可选独立批：删除观察迁移，不提前纳入前三批完成标准。

## 固定行为到用例映射

以下是计划用例名称，尚未创建；实施时填写真实路径，不得标为已有测试。

| 不变量 | 计划用例/断言 ID | 层级 | 必须结果 |
|---|---|---|---|
| KR01 | ready_classification_matrix | 组件 | Pending/Ready/Succeeded/三类失败与现有语义一致 |
| KR02 | named_pod_wrong_uid_or_owner | 组件＋API | 同名替代/别族不能成功 |
| KR03 | deadline_includes_initial_list_and_reconnect | API | LIST 卡住、反复重连仍在原预算内结束 |
| KR04 | cancel_drops_watch_and_preserves_shared_worker | API＋协调器 | 客户端流释放，共享创建继续 |
| KR05 | watch_410_relist_and_403_fail_fast | API | 410 重 list/RV 接续；403 明确失败 |
| KR05 | eof_fragmented_event_and_decode_failure | API | 拆包正常、EOF 续接、坏数据明确失败 |
| KR06 | observation_timeout_retains_operation_fence | 生命周期组件 | 无释放、无重复写、保留未知结果 |
| KR07 | restart_old_ready_and_sts_replacement | API＋K8s | 旧 Pod 不确认重启，STS 替换冲突 |
| KR07 | stop_wake_identity_and_replicas | API＋K8s | 真实停止/新 Pod 唤醒，归属正确 |
| KR08 | replacement_blocks_following_cleanup | API，D 批 | UID 替换后没有任何后续破坏请求 |
| KR09 | event_403_429_timeout_full_queue_shutdown | API＋组件 | 业务结果不变，丢弃计数正确、排空有界 |
| KR10 | event_api_group_identity_redaction | API＋清单 | API group/UID 正确、无敏感内容、RBAC 匹配 |
| KR11 | status_only_ready_update_is_delivered | API | generation 不变的 Ready 被处理 |
| KR12 | userapp_existing_contracts | 真实 K8s | hot 内容 A/B、身份、HTTP 信封均保留 |

## 每轮证据模板

```text
run_id:
date:
source_head_and_worktree_fingerprint:
image_digest_and_deployment_receipt:
test_source_fingerprint:
namespace_from_config:
commands_and_exit_codes:
planned_assertions / executed / passed / failed / skipped / aborted:
logs_and_report_paths:
before_after_owned_resource_identities:
cleanup_result_and_retained_agent_pvcs:
performance_samples_and_limitations:
remaining_blockers:
```

目前所有实现门禁、K8s 业务验收和性能结果：**未执行**。前一轮原始 watch 通路成功不计入 KR01–KR12 的通过数。
