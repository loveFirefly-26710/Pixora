-- 公告已读：按「账号 + 公告版本」记一次。
-- 换设备、清浏览器缓存都不会让已经读过的公告再弹；公告正文变化会产生新版本。
CREATE TABLE announcement_reads (
    user_id BIGINT      NOT NULL,
    version VARCHAR(64) NOT NULL,
    read_at VARCHAR(32) NOT NULL,
    PRIMARY KEY (user_id, version),
    CONSTRAINT fk_announcement_reads_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
