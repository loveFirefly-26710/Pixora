-- 回收站：删相册与删图片都先软删，可恢复；到期由后台清理（与 sqlite 版同构）。

ALTER TABLE albums ADD COLUMN deleted_at TEXT;
ALTER TABLE images ADD COLUMN deleted_by_album INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_albums_live    ON albums(owner_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_images_deleted ON images(owner_id, deleted_at) WHERE deleted_at IS NOT NULL;
