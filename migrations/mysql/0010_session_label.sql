-- 会话（cookie 记录）可以由用户重命名，方便认出「哪台设备」。
-- 设备名来自 User-Agent，用户没法改；备注可以改，也不影响鉴权。
ALTER TABLE sessions ADD COLUMN label VARCHAR(120) NULL;
