-- 两列：上传来源。访客上传要留痕，登录用户顺手也记上。
--
-- 「谁传的」这件事以前只有归属账号知道；站点允许游客上传之后，同一个访客
-- 账号底下会混着很多人的图，出问题时要能回答「哪一张、什么时候、从哪个 IP、
-- 什么终端传的」。这两列只在 upload 那一步写入，之后不再改。

ALTER TABLE images ADD COLUMN upload_ip TEXT;
ALTER TABLE images ADD COLUMN upload_agent TEXT;
