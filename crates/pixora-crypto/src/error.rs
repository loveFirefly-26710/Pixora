use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("系统随机源不可用：{0}")]
    RandomUnavailable(String),

    #[error("口令哈希失败：{0}")]
    PasswordHash(String),

    #[error("口令校验失败：{0}")]
    PasswordVerify(String),

    #[error("口令强度不足：{0}")]
    WeakPassword(String),

    #[error("主密钥格式无效：{0}")]
    InvalidKey(String),

    #[error("加解密失败：{0}")]
    Cipher(String),

    #[error("密文格式无效：{0}")]
    MalformedCiphertext(String),

    #[error("Base32 解码失败：{0}")]
    Base32(String),

    #[error("TOTP 校验失败：{0}")]
    Totp(String),
}

pub type Result<T> = std::result::Result<T, CryptoError>;
