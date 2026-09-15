-- 用户绑定移除（spec/userapp-remove-user-id-binding）：userapp_metadata
-- 不再持有归属用户。列数据不迁移（应用共享，无消费方），直接删除。
ALTER TABLE userapp_metadata DROP COLUMN IF EXISTS user_id;
