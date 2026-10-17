-- 回收站：删相册与删图片都先软删，可恢复；到期由后台清理（与 sqlite 版同构）。
-- MySQL 不支持带 WHERE 的局部索引，这里用普通联合索引。

-- 时间列一律 VARCHAR(32)：TEXT 上建不了索引（1170）
ALTER TABLE albums ADD COLUMN deleted_at VARCHAR(32);
-- 与其余整数列一致用 BIGINT：代码统一按 i64 读
ALTER TABLE images ADD COLUMN deleted_by_album BIGINT NOT NULL DEFAULT 0;

CREATE INDEX idx_albums_live    ON albums(owner_id, deleted_at);
CREATE INDEX idx_images_deleted ON images(owner_id, deleted_at);
