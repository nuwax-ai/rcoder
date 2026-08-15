# rcoder-pg 实施顺序（Phase 1）

批准版设计讨论结论（2026-08-14，plan mode 五轮修订定稿）：

1. **抽象方式**：枚举静态分发（用户拍板否决 dyn；泛型因调用面传染否决）。
2. **模块归属**：新 crate `rcoder-storage` 承载整个存储层（用户拍板拆 crate）；
   契约（trait/数据类型）进 shared_types（用户提出"共享领域模块"——结论：不加
   第二个领域 crate，强化既有 shared_types 并固化约定）。
3. **Phase 1 范围**：三类内存真源一起进（ProjectAdapter + AppActivityRegistry +
   PublishTaskStore）。
4. **DDL 全字段 COMMENT ON**（用户要求）。
5. **PG 配置双通道**：config.yml `[storage.postgres]` 离散字段 + url 逃生口 +
   `RCODER_PG_*` env 逐字段覆盖（用户要求补全）。
6. **测试 PG**：测试集群起单实例 StatefulSet（Phase 3 换 CNPG）。

执行顺序：U 线（OpenAPI，趁未上线）→ M1 迁移 → M2 trait/枚举 → M3 PgStore →
M4 行为分叉 → M5 Activity/Publish → M6 集群验证。

各步完成状态与细节见 [tasks.md](tasks.md)。
