-- 回收站：删相册与删图片都先软删，可恢复；到期由后台清理。
--
-- 两个新列：
--   albums.deleted_at      相册被移进回收站的时间（NULL = 正常）
--   images.deleted_by_album 这张图是「随相册一起」进的回收站：
--                           恢复相册时只带回这一批，不碰用户早先单独删掉的图

ALTER TABLE albums ADD COLUMN deleted_at TEXT;
ALTER TABLE images ADD COLUMN deleted_by_album INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_albums_live     ON albums(owner_id, deleted_at);
CREATE INDEX idx_images_deleted  ON images(owner_id, deleted_at) WHERE deleted_at IS NOT NULL;
