-- 一张表：账号的第三方登录方式（GitHub / Google / QQ / 微信 / 手机号）。
--
-- 邮箱不在这张表里：它是账号本身（`users.email`）——必需、不可解绑，
-- 也是找回账号的凭据。这张表只放「额外挂了哪些门进来」。
--
-- `(provider, subject)` 唯一：一个第三方身份只能挂到一个账号上。`subject` 是
-- 对方的用户标识（GitHub 的 id、Google 的 sub、QQ/微信的 openid、手机号），
-- 一律转成字符串存。`display` 是界面上给用户看的（昵称、掩码手机号），
-- `meta` 存 JSON 原样（头像地址、unionid 之类），解析留给用它的那一层。
CREATE TABLE user_identities (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider   TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    display    TEXT,
    meta       TEXT,
    created_at TEXT    NOT NULL,
    updated_at TEXT    NOT NULL,
    UNIQUE (provider, subject),
    UNIQUE (user_id, provider)
);
CREATE INDEX idx_user_identities_user ON user_identities(user_id);
