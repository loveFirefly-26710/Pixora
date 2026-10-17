-- 上传策略的按用户覆盖：单文件上限、单批张数与并发数本来就在策略表里，
-- 这里补上剩下两项——
--   max_pixels  像素上限（0 = 继承上一层的值）
--   formats     允许的图片格式，逗号分隔（NULL / 空串 = 继承）；与
--               settings 里的 upload.formats 用同一种写法，两处不用互相翻译
--
-- 约定：数值 0 与空格式串都表示「跟随上一层」，解析顺序见
-- crates/pixora-core/src/uploads.rs 的 effective_upload_limits。

ALTER TABLE rate_limit_policies ADD COLUMN max_pixels BIGINT NOT NULL DEFAULT 0;
ALTER TABLE rate_limit_policies ADD COLUMN formats TEXT;
