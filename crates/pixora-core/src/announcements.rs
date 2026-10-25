//! 公告已读的服务层入口。
//!
//! 记录本身只在仓储里；这里集中做版本格式校验，避免路由把任意长字符串塞进库。

use crate::error::{AppError, Result};
use crate::AppState;

impl AppState {
    /// 这个账号是否已经确认过该版本的公告。
    pub async fn announcement_read(&self, user_id: i64, version: &str) -> Result<bool> {
        let version = normalize_version(version)?;
        Ok(self.repo.announcement_read_by(user_id, version).await?)
    }

    /// 记一次已读。重复调用不会报错。
    pub async fn mark_announcement_read(&self, user_id: i64, version: &str) -> Result<()> {
        let version = normalize_version(version)?;
        Ok(self
            .repo
            .mark_announcement_read(user_id, version, chrono::Utc::now())
            .await?)
    }

    /// 当前公告版本有多少账号读过。
    pub async fn announcement_read_count(&self, version: &str) -> Result<i64> {
        let version = normalize_version(version)?;
        Ok(self.repo.announcement_read_count(version).await?)
    }
}

fn normalize_version(version: &str) -> Result<&str> {
    let version = version.trim();
    if version.is_empty() || version.len() > 64 {
        return Err(AppError::bad_request("公告版本不合法"));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn announcement_reads_are_per_user_and_version() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let user = crate::tests_support::make_user(&state, "reader@example.com", "8801").await;

        assert!(!state.announcement_read(user.id, "v1").await.unwrap());
        state.mark_announcement_read(user.id, "v1").await.unwrap();
        assert!(state.announcement_read(user.id, "v1").await.unwrap());
        assert!(!state.announcement_read(user.id, "v2").await.unwrap());
        // 重复确认是幂等的
        state.mark_announcement_read(user.id, "v1").await.unwrap();
        assert_eq!(state.announcement_read_count("v1").await.unwrap(), 1);
        assert_eq!(state.announcement_read_count("v2").await.unwrap(), 0);
    }
}
