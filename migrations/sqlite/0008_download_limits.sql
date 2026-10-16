-- 两列：下载频率限制。
--
-- 「下载」指的是带 `?download=1` 的直链请求（附件的下载）。图片直链本身
-- 是页面渲染的一部分，不能按下载计数，否则打开一次广场就把额度耗光了。
--
-- 与其余限流项同一套语义：`0` 表示「跟随上一层」——用户级覆盖里 0 就是
-- 跟着全局默认走；全局那行是 0 就是不限。未登录的游客没有账号，限额放在
-- 系统设置里（`download.anon_per_minute` / `download.anon_per_day`）。
ALTER TABLE rate_limit_policies ADD COLUMN download_per_minute INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rate_limit_policies ADD COLUMN download_per_day INTEGER NOT NULL DEFAULT 0;
