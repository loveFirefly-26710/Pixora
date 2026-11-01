-- 延迟删除：用户删掉的会话与令牌不立刻从库里消失。
--
-- 删除时只打两个标记：
--   deleted_at  用户点「删除」的时间（列表里从此看不到，鉴权立刻失效）
--   deleted_by  谁删的（user / admin / system），给安全审计用
-- revoked_at 仍然保留原来的语义（下线/撤销），两者可以同时有值。
-- 保留期到了由后台任务按 deleted_at 物理删除，库里从不存令牌明文。
ALTER TABLE sessions ADD COLUMN deleted_at VARCHAR(40) NULL;
ALTER TABLE sessions ADD COLUMN deleted_by VARCHAR(16) NULL;
ALTER TABLE api_tokens ADD COLUMN deleted_at VARCHAR(40) NULL;
ALTER TABLE api_tokens ADD COLUMN deleted_by VARCHAR(16) NULL;

-- MySQL 不支持部分索引，普通索引即可：清理任务按 deleted_at 扫
CREATE INDEX idx_sessions_deleted ON sessions(deleted_at);
CREATE INDEX idx_api_tokens_deleted ON api_tokens(deleted_at);
