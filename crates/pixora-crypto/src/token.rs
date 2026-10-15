//! 令牌与随机字串。
//!
//! 三类令牌各有一套前缀，日志和界面上一眼能分辨是哪种：
//! 会话 `pxs_`、API `px_live_`、CSRF 无前缀（它只活在 Cookie 与请求头里）。
//!
//! 库里存的都是 SHA-256 哈希（见 `token_hash`），明文只在签发那一刻出现一次。

use subtle::ConstantTimeEq;

use crate::error::Result;

/// 会话令牌前缀。
pub const SESSION_PREFIX: &str = "pxs_";
/// API 令牌前缀。
pub const API_PREFIX: &str = "px_live_";
/// 重置链接令牌前缀。
pub const RESET_PREFIX: &str = "pxr_";

fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    crate::fill_random(&mut buf)?;
    Ok(buf)
}

/// 32 字节随机数，十六进制编码。
pub fn random_hex(byte_len: usize) -> Result<String> {
    Ok(hex::encode(random_bytes(byte_len)?))
}

/// 新会话令牌。256 位熵，足够抵御在线猜测与离线碰撞。
pub fn new_session_token() -> Result<String> {
    Ok(format!("{SESSION_PREFIX}{}", random_hex(32)?))
}

/// 新 API 令牌。只在生成时返回明文。
pub fn new_api_token() -> Result<String> {
    Ok(format!("{API_PREFIX}{}", random_hex(24)?))
}

/// 新重置链接令牌。
///
/// 链接里带的是它，不是邮箱与验证码：那些是明文参数，转发一次链接就等于
/// 把「谁的账号 + 当前有效的验证码」一起递了出去。令牌是随机数，库里也只存
/// 哈希，用一次就废。
pub fn new_reset_token() -> Result<String> {
    Ok(format!("{RESET_PREFIX}{}", random_hex(32)?))
}

/// 新 CSRF 令牌（双提交 Cookie 用）。
pub fn new_csrf_token() -> Result<String> {
    random_hex(32)
}

/// 令牌在库里的存储形态：SHA-256 十六进制。
///
/// 用 SHA-256 而不是 Argon2 是有意的：令牌本身就是高熵随机串，
/// 不存在字典攻击的空间，而每次 API 请求都要算一次，必须便宜。
pub fn token_hash(token: &str) -> String {
    crate::hash::sha256_hex(token.as_bytes())
}

/// 邮箱验证码。数字便于用户在邮件里读抄。
pub fn new_numeric_code(len: usize) -> Result<String> {
    let mut out = String::with_capacity(len);
    // 拒绝采样，避免 `% 10` 带来的轻微偏置
    const LIMIT: u32 = u32::MAX - (u32::MAX % 10);
    while out.len() < len {
        let mut buf = [0u8; 4];
        crate::fill_random(&mut buf)?;
        let value = u32::from_le_bytes(buf);
        if value < LIMIT {
            out.push(char::from(b'0' + (value % 10) as u8));
        }
    }
    Ok(out)
}

/// 用户 UID：对外可见的短标识，同时作为物理目录的一级分类。
///
/// 从 4 位起步，冲突由调用方重试；位数越大空间越松。
pub fn new_uid(digits: usize) -> Result<String> {
    let digits = digits.clamp(4, 8);
    let mut out = String::with_capacity(digits);
    // 首位不能是 0，否则 UID 会被当成八进制或直接丢位
    let mut first = [0u8; 1];
    crate::fill_random(&mut first)?;
    out.push(char::from(b'1' + (first[0] % 9)));
    let rest = new_numeric_code(digits - 1)?;
    out.push_str(&rest);
    Ok(out)
}

/// 定长比较。用于比较 CSRF Token、验证码这类需要抵抗时序侧信道的值。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).unwrap_u8() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_prefixed() {
        let a = new_session_token().unwrap();
        let b = new_session_token().unwrap();
        assert!(a.starts_with(SESSION_PREFIX));
        assert_ne!(a, b);
        assert_eq!(a.len(), SESSION_PREFIX.len() + 64);

        let api = new_api_token().unwrap();
        assert!(api.starts_with(API_PREFIX));
    }

    #[test]
    fn token_hash_is_stable() {
        let token = new_api_token().unwrap();
        assert_eq!(token_hash(&token), token_hash(&token));
        assert_eq!(token_hash(&token).len(), 64);
        assert_ne!(token_hash(&token), token_hash("other"));
    }

    #[test]
    fn numeric_code_shape() {
        let code = new_numeric_code(6).unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn uid_has_no_leading_zero() {
        for _ in 0..64 {
            let uid = new_uid(4).unwrap();
            assert_eq!(uid.len(), 4);
            assert!(!uid.starts_with('0'));
        }
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
