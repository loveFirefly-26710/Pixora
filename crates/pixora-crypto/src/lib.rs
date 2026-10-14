//! Pixora 的加密与令牌原语。
//!
//! 这一层只做纯粹的计算：哈希、口令、对称加密、随机令牌、TOTP。
//! 它不认识数据库，也不认识 HTTP——上层的凭据加解密、会话签发都从这里取料。

pub mod base32;
pub mod cipher;
pub mod error;
pub mod hash;
pub mod password;
pub mod token;
pub mod totp;

pub use cipher::MasterKey;
pub use error::CryptoError;

/// 统一的随机源。全局共享一个实现，避免各处各自引入 rand 家族的不同版本。
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::fill(buf).map_err(|e| CryptoError::RandomUnavailable(e.to_string()))
}
