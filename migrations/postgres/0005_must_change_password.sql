-- 一列：管理员直接给用户设了新密码时打的标记。
--
-- 管理员手输的密码要经由邮件送达，中间可能落在聊天记录里；不收一次
-- 「还得自己改」的约束，这个密码就会一直用下去。打上标记之后：
--
--   * 登录后只有 /me、/me/password 与登出可用，其余接口一律 403；
--   * 前端进站直接落到改密码页，改完标记清除。
--
-- 走自助重置（邮件里的链接/验证码）设的密码不打这个标记：那是用户自己选的。

-- 这一列的方言约定是 BIGINT（0/1），与其余布尔列一致：代码层统一按 i64 解码，
-- 声明成 BOOLEAN 会让 sqlx 在读取时报「Rust type i64 is not compatible with
-- SQL type BOOLEAN」。
ALTER TABLE users ADD COLUMN must_change_password BIGINT NOT NULL DEFAULT 0;
