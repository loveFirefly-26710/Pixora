//! 访客账号。
//!
//! 站点可以允许未登录的人上传（后台「拒绝游客上传」关掉时）。上传结果总要有个
//! 归属：配额、回收站、内容审核、按用户的频率限制，全都挂在账号上。所以游客的
//! 图片统一落到一个固定的 visitor 账号里，另外在图片行上记下这次上传的 IP 与
//! 终端——「谁传的」要能查得到。
//!
//! 这个账号不能登录：密码哈希写成一个永远验不过去的值，邮箱也是保留域，
//! 登录、改密、改邮箱、改角色这几条路各自还有一道显式的拒绝（见
//! `ensure_not_guest`）——它是系统账号，不该由谁「顺手」把这扇门打开。

use pixora_repo::model::{role, status, User};
use pixora_repo::repo::users::NewUser;

use crate::error::{AppError, Result};
use crate::AppState;

/// 访客账号的 UID。占一个不会被随机 UID 撞到的号段（真实 UID 从 1000 起）。
pub const GUEST_UID: &str = "0000";
/// 访客账号的邮箱。`.invalid` 是 RFC 2606 保留给「不存在的域」的，发信也发不出去。
pub const GUEST_EMAIL: &str = "visitor@pixora.invalid";
/// 永远验不过去的密码哈希：这个账号也就没有可用的口令。
const UNUSABLE_PASSWORD: &str = "!";

/// 这个 UID 是不是访客账号的。
pub fn is_guest_uid(uid: &str) -> bool {
    uid == GUEST_UID
}

/// 这个账号是不是访客账号。
pub fn is_guest(user: &User) -> bool {
    is_guest_uid(&user.uid)
}

/// 拒绝一切「把访客账号当成人的账号来用」的操作。
///
/// 访客账号是游客上传的归属地：配额、回收站、审计都记在它名下，但它不是给人登录
/// 的账号。一旦它能被登进去，进去的人看到的是全站游客的上传与回收站；管理员把它
/// 改成管理员角色，更是直接进了后台。所以登录、设/改密码、改邮箱、改角色四处都
/// 显式拒绝，而不是指望「密码哈希验不过去」这一个巧合。
pub fn ensure_not_guest(user: &User) -> Result<()> {
    if is_guest(user) {
        return Err(AppError::forbidden(
            "访客账号是系统账号，不能登录，也不能修改邮箱、密码或角色",
        ));
    }
    Ok(())
}

impl AppState {
    /// 站点是否允许游客上传。
    pub async fn guest_upload_enabled(&self) -> Result<bool> {
        let site = self.settings.site(self.config.base_url.as_deref()).await?;
        Ok(!site.require_login_to_upload)
    }

    /// 取访客账号：站点没开游客上传时返回 `None`。
    ///
    /// 访客账号被暂停或封禁时同样返回 `None`——管理员想按住游客上传，
    /// 直接处置那个账号就行，不必再翻设置。
    pub async fn guest_uploader(&self) -> Result<Option<User>> {
        if !self.guest_upload_enabled().await? {
            return Ok(None);
        }
        let guest = self.ensure_guest_user().await?;
        if guest.status != status::ACTIVE {
            return Ok(None);
        }
        Ok(Some(guest))
    }

    /// 找访客账号，没有就建一个（连带默认相册）。
    ///
    /// 建的时候用站点当前的默认配额：管理员之后可以在「用户管理」里单独给
    /// 访客账号调配额，那条路走的是普通用户那套，不需要另开入口。
    pub async fn ensure_guest_user(&self) -> Result<User> {
        if let Some(user) = self.repo.find_user_by_email(GUEST_EMAIL).await? {
            return Ok(user);
        }

        let (quota_default, _) = self.settings.quota_values().await?;
        let user = self
            .repo
            .create_user(&NewUser {
                uid: GUEST_UID.to_string(),
                email: GUEST_EMAIL.to_string(),
                password_hash: UNUSABLE_PASSWORD.to_string(),
                role: role::USER.to_string(),
                status: status::ACTIVE.to_string(),
                // 这个邮箱收不到信（保留域），标记成未验证只是如实反映
                email_verified: false,
                quota_bytes: quota_default,
            })
            .await;

        let user = match user {
            Ok(user) => user,
            Err(err) if err.is_unique_violation() => {
                // 两个请求同时发现「还没有访客账号」时，后到的那个回读一次
                self.repo
                    .find_user_by_email(GUEST_EMAIL)
                    .await?
                    .ok_or_else(|| AppError::internal("访客账号创建冲突，请重试"))?
            }
            Err(err) => return Err(err.into()),
        };

        self.repo.insert_default_album(user.id, "访客相册").await?;
        tracing::info!(uid = %user.uid, "已创建访客账号，游客上传会落到它名下");
        Ok(user)
    }
}
