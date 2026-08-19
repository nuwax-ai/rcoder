-- UserApp 应用业务元数据:仅存集群不持有的字段(desired 运行字段 image/env/resources
-- 等以 K8s/Docker 集群为事实源,本表不镜像)。支撑 POST /apps/query 的 name/created_at
-- 过滤(PG 模式),调用方可不再自存应用元数据。
CREATE TABLE userapp_metadata (
    app_id     text PRIMARY KEY,          -- UserApp 应用 ID(app- 前缀,与集群资源名一致)
    name       text,                      -- 业务名称(仅元数据,集群不持有)
    tenant_id  text,                      -- 租户 ID(冗余自资源 label,便于查询过滤)
    space_id   text,                      -- 空间 ID(同上)
    created_at timestamptz NOT NULL,      -- 业务首次创建时间(集群 creationTimestamp 重建会刷新,此列不刷新)
    updated_at timestamptz NOT NULL DEFAULT now()  -- 元数据最后更新时间(upsert 刷新)
);

COMMENT ON TABLE userapp_metadata IS 'UserApp 应用业务元数据:仅存集群不持有的字段(name/租户/业务创建时间);desired 运行字段以 K8s/Docker 集群为事实源,本表不镜像';
COMMENT ON COLUMN userapp_metadata.app_id IS 'UserApp 应用 ID(app- 前缀,与集群资源名一致;delete/purge 保留行支持误删找回,storage/destroy 删行)';
COMMENT ON COLUMN userapp_metadata.name IS '业务名称(仅元数据,集群不持有;query name 过滤数据源)';
COMMENT ON COLUMN userapp_metadata.tenant_id IS '租户 ID(冗余自资源 label rcoder.io/tenant,便于查询过滤)';
COMMENT ON COLUMN userapp_metadata.space_id IS '空间 ID(冗余自资源 label rcoder.io/space,同上)';
COMMENT ON COLUMN userapp_metadata.created_at IS '业务首次创建时间(upsert ON CONFLICT 不更新;集群 creationTimestamp 在同 app_id 重建时会刷新,本列不会)';
COMMENT ON COLUMN userapp_metadata.updated_at IS '元数据最后更新时间(create/update upsert 时刷新)';
