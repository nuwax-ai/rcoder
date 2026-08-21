-- userapp_metadata 加归属用户列:部署访问 URL 统一四段形态
-- (/proxy/apps/{user_id}/{app_id}/{port}/{*path})与未来"我的应用"过滤/归属校验的数据源。
-- 存量行为空(可空列,create 后新行必填)。
ALTER TABLE userapp_metadata ADD COLUMN user_id text;
COMMENT ON COLUMN userapp_metadata.user_id IS '归属用户 ID(仅元数据,集群不持有;部署访问 URL /proxy/apps/{user_id}/... 与归属过滤数据源)';
