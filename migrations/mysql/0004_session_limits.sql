-- 四件小事，各占一列：
--
--   sessions.expires_at         「记住我」签发的会话写绝对到期时间；普通会话留空，
--                               有效性由「距上次活动多久」判定（见 pixora-core/src/session.rs）
--   users.auto_unsuspend_at     内容审核自动暂停的到期时间。到点自动恢复；
--                               管理员手工暂停的行这里永远是空的，不会被误恢复
--   rate_limit_policies.max_width / max_height
--                               像素上限从「一个总像素数」拆成长宽两个上限，
--                               0 依旧表示「跟随上一层」

ALTER TABLE sessions ADD COLUMN expires_at VARCHAR(32);
ALTER TABLE users ADD COLUMN auto_unsuspend_at VARCHAR(32);
ALTER TABLE rate_limit_policies ADD COLUMN max_width BIGINT NOT NULL DEFAULT 0;
ALTER TABLE rate_limit_policies ADD COLUMN max_height BIGINT NOT NULL DEFAULT 0;

CREATE INDEX idx_users_auto_unsuspend ON users(auto_unsuspend_at);
