# S12：历史操作规模下的恢复扫描

## 实测结论

在独立 PG17 fixture 内使用当时真实 UserApp SQL 基线，保留所有 FK/CHECK，插入 300,000 条终态历史、200 个 app 各一条未完成操作及当前 prod 槽位、300 条租约（200 未完成与 100 待清理终态）。每个查询执行五次 `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)`，取执行耗时中位数。

| 查询 | 原基线 | 临时加入部分索引后 | shared buffer blocks |
|---|---:|---:|---:|
| 未完成操作首屏 100 条 | 34.920 ms | 0.076 ms | 8904 → 87 |
| 未完成操作尾部游标 | 25.728 ms | 0.086 ms | 8040 → 11 |
| 终态租约首屏 | 1.861 ms | 2.478 ms | 1447 → 1447 |

原首屏使用 Parallel Seq Scan，每个 worker 过滤 100,000 条历史。原索引 `(state, app_id, operation_id)` 不匹配实际 `terminal_at_us IS NULL` 谓词。候选部分索引后首屏使用 Index Scan；尾游标只访问 12 条未完成候选。候选索引 32 KB。租约查询计划未改变，耗时差属于本轮波动，不据此追加索引。

证据：`/tmp/rcoder-s12-f8dbe0ce8411/summary.json` 和六份 before/after JSON；含 schema SHA256、HEAD、镜像身份与完整计划。`cleanup.txt` 确认仅本轮所属容器及匿名卷已移除。该测量是缓存命中条件下的扫描证据，不是生产延迟承诺。首次 fixture 被 PG 临时初始化 socket 的关闭窗口阻断，在执行 schema 前失败并清理；修正为等待正式 TCP listener 后取得上述成功结果。

## 最小修复

PG 与 Turso 的 UserApp v1 初始化 SQL 均新增：

```sql
CREATE INDEX userapp_operations_unfinished
ON userapp_operations(operation_id)
WHERE terminal_at_us IS NULL;
```

不改变查询、状态机和现有索引。现有 catalog 摘要已覆盖完整 index DDL，包括谓词，因此无需新建例外目录或放宽比较；新增回归验证缺失索引和同名错误谓词均拒绝初始化。Turso 回归另以 10,000 条历史和一条未完成记录核对实际 `EXPLAIN QUERY PLAN` 与返回结果。

**基线尚未发布。** 此变更按既定四份首次初始化 SQL 规则直接更新 UserApp 两份基线，checksum 随之变化。使用旧 checksum 的本轮开发数据库会被明确拒绝；不增加旧 checksum 绕过，不删除或改写旧数据。统一验证使用独立新目录/数据库，原有开发库保留。正式环境的首次基线切换仍按已批准的资源核验与重建方案执行。

## 可复现实测入口

```bash
python3 tools/test_storage_recovery_scan.py --run
# 默认真实 PG17、本地已有 postgres:17 镜像、300000 条历史；不拉取镜像。
# 小规模诊断可加 --history-rows 10000，不能代替默认规模验收。
```

工具创建唯一带所属标签的容器，无宿主端口映射，不接收现有数据库 DSN。只在该 fixture 中临时移除新索引以复现旧计划，之后恢复当前基线索引；比较查询结果完全一致并断言未完成扫描使用部分索引，输出五轮完整计划、版本、schema 摘要、索引大小和清理记录。退出时核验标签后删除所属容器及匿名卷。全部证据留在打印的临时目录。耗时是观察数据，不使用固定毫秒阈值制造易抖动的门禁。

## 验证状态

原始 PG17 比较探针已执行并通过，数据见上表。正式仓库工具、两份 SQL 和新增 Turso/catalog 回归已写入；Python AST、定向 rustfmt、`git diff --check` 通过。按统一验证安排，本次尚未运行 Cargo，也未执行新仓库工具；待集中记录实际 nextest 与工具结果，不沿用旧 schema 的组件结果宣称本轮通过。
