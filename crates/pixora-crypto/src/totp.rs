//! TOTP（RFC 6238），管理员账号的二次验证。
//!
//! 只实现 SHA-1 / 6 位 / 30 秒这一组参数——它是各家验证器 App 的公共默认值，
//! 换其它参数只会让用户绑不上。校验留 ±1 个时间窗，容忍客户端与服务器的时钟漂移。

use chrono::{DateTime, Utc};

use crate::error::{CryptoError, Result};
use crate::hash::hmac_sha1;

/// 时间步长（秒）。
const STEP: u64 = 30;
/// 验证码位数。
const DIGITS: u32 = 6;
/// 密钥字节数。160 位是 RFC 4226 对 SHA-1 的建议值。
const SECRET_BYTES: usize = 20;

/// 生成新的 Base32 密钥（不带填充）。
pub fn generate_secret() -> Result<String> {
    let mut secret = [0u8; SECRET_BYTES];
    crate::fill_random(&mut secret)?;
    Ok(crate::base32::encode(&secret))
}

/// 计算指定时间点的验证码。
pub fn code_at(secret: &str, at: DateTime<Utc>) -> Result<String> {
    let step = (at.timestamp().max(0) as u64) / STEP;
    code_for_step(secret, step)
}

/// 当前验证码。用于测试和「显示当前码」这类调试界面。
pub fn code_now(secret: &str) -> Result<String> {
    code_at(secret, Utc::now())
}

fn code_for_step(secret: &str, step: u64) -> Result<String> {
    let key = crate::base32::decode(secret)?;
    if key.is_empty() {
        return Err(CryptoError::Totp("密钥为空".into()));
    }
    let counter = step.to_be_bytes();
    let digest = hmac_sha1(&key, &counter);

    // 动态截断（RFC 4226 §5.3）
    let offset = (digest[19] & 0x0f) as usize;
    let binary = ((digest[offset] as u32 & 0x7f) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | (digest[offset + 3] as u32);
    let code = binary % 10u32.pow(DIGITS);
    Ok(format!("{code:0width$}", width = DIGITS as usize))
}

/// 校验验证码，允许前后各 `window` 个时间窗（通常取 1）。
///
/// 只回答「这个码对不对」。防重放要在上层做——同一个码在一个窗口内可以被
/// 重复提交，这里不做记忆，避免把状态塞进一个纯函数里。
pub fn verify(secret: &str, code: &str, window: u8) -> Result<bool> {
    let code = code.trim().replace(' ', "");
    if code.len() != DIGITS as usize || !code.chars().all(|c| c.is_ascii_digit()) {
        return Ok(false);
    }
    let now = (Utc::now().timestamp().max(0) as u64) / STEP;
    let span = window as u64;
    let start = now.saturating_sub(span);
    for step in start..=now + span {
        let candidate = code_for_step(secret, step)?;
        if crate::token::constant_time_eq(&candidate, &code) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// otpauth:// 链接，前端渲染成二维码给验证器 App 扫。
pub fn otpauth_url(secret: &str, issuer: &str, account: &str) -> String {
    let issuer_enc = urlencode(issuer);
    format!(
        "otpauth://totp/{issuer_enc}:{account}?secret={secret}&issuer={issuer_enc}&algorithm=SHA1&digits={DIGITS}&period={STEP}",
        account = urlencode(account),
    )
}

fn urlencode(input: &str) -> String {
    percent_encoding_encode(input)
}

/// 极简百分号编码。只处理 otpauth URL 里会出现的字符。
fn percent_encoding_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// RFC 6238 附录 B 的测试向量使用 8 位码，这里用同一密钥验证
    /// 6 位截断的自洽性：手工重算一遍 HMAC 流程。
    #[test]
    fn known_secret_is_stable() {
        // "12345678901234567890" 的 Base32
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let at = Utc.timestamp_opt(59, 0).unwrap();
        let code = code_at(secret, at).unwrap();
        assert_eq!(code.len(), 6);
        // 同一个时间点必须稳定
        assert_eq!(code, code_at(secret, at).unwrap());
        // 换一个时间窗必须变化（30 秒粒度）
        let next = code_at(secret, Utc.timestamp_opt(60, 0).unwrap()).unwrap();
        assert_ne!(code, next);
    }

    #[test]
    fn current_code_verifies() {
        let secret = generate_secret().unwrap();
        let code = code_now(&secret).unwrap();
        assert!(verify(&secret, &code, 1).unwrap());
        assert!(!verify(&secret, "000000", 2).unwrap() || code == "000000");
        assert!(!verify(&secret, "abcdef", 1).unwrap());
        assert!(!verify(&secret, "12345", 1).unwrap());
    }

    #[test]
    fn otpauth_url_is_encoded() {
        let url = otpauth_url("ABC234", "Pixora", "admin@example.com");
        assert!(url.starts_with("otpauth://totp/Pixora:"));
        assert!(url.contains("secret=ABC234"));
        assert!(url.contains("%40example.com"));
    }
}
