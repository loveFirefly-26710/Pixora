//! 口令哈希。Argon2id 是唯一允许的算法。
//!
//! 参数取 OWASP 推荐的一组折中值（m=19MiB, t=2, p=1）：图床的登录频率不高，
//! 单次 20 毫秒的开销可以接受，换来的是离线爆破成本的量级提升。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::error::{CryptoError, Result};

/// Argon2id 的内存开销（KiB）。
const MEMORY_KIB: u32 = 19 * 1024;
/// 迭代次数。
const ITERATIONS: u32 = 2;
/// 并行度。保持 1，避免在容器里按 CPU 核数放大内存占用。
const PARALLELISM: u32 = 1;

fn argon2() -> Result<Argon2<'static>> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None)
        .map_err(|e| CryptoError::PasswordHash(e.to_string()))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// 生成口令哈希（PHC 字符串，自带盐与参数）。
pub fn hash_password(plain: &str) -> Result<String> {
    let mut salt_bytes = [0u8; 16];
    crate::fill_random(&mut salt_bytes)?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| CryptoError::PasswordHash(e.to_string()))?;
    let hash = argon2()?
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| CryptoError::PasswordHash(e.to_string()))?;
    Ok(hash.to_string())
}

/// 校验口令。校验用的是哈希串里记录的参数，与当前默认参数无关，
/// 这样以后调参不会让存量口令失效。
pub fn verify_password(plain: &str, stored: &str) -> Result<bool> {
    let parsed = PasswordHash::new(stored).map_err(|e| CryptoError::PasswordVerify(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok())
}

/// 口令强度校验。规则来自注册页的提示：至少 10 位，含大小写与数字。
///
/// 不做"必须含特殊符号"这一条——它把用户推向 `Passw0rd!` 这类可预测的写法，
/// 长度才是真正有效的那个变量。
pub fn check_strength(plain: &str) -> Result<()> {
    if plain.chars().count() < 10 {
        return Err(CryptoError::WeakPassword("口令至少 10 位".into()));
    }
    if plain.chars().count() > 128 {
        return Err(CryptoError::WeakPassword("口令不能超过 128 位".into()));
    }
    let has_lower = plain.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = plain.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = plain.chars().any(|c| c.is_ascii_digit());
    if !(has_lower && has_upper && has_digit) {
        return Err(CryptoError::WeakPassword(
            "口令需同时包含大写字母、小写字母和数字".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let stored = hash_password("Pixora2026Abc").unwrap();
        assert!(stored.starts_with("$argon2id$"));
        assert!(verify_password("Pixora2026Abc", &stored).unwrap());
        assert!(!verify_password("Pixora2026Abd", &stored).unwrap());
    }

    #[test]
    fn strength_rules() {
        assert!(check_strength("Short1A").is_err());
        assert!(check_strength("alllowercase1").is_err());
        assert!(check_strength("ALLUPPERCASE1").is_err());
        assert!(check_strength("NoDigitsHere").is_err());
        assert!(check_strength("ValidPass123").is_ok());
    }
}
