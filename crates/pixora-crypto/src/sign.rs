//! 主密钥签名的小载荷：`base64url(json).hmac`。
//!
//! 用于服务端「自己发、自己验」的一次性凭据——OAuth 的 `state`、注册补全邮箱的
//! 令牌这类。它不是加密：载荷本身是明文（不该往里塞敏感信息），要的只是
//! 「没有被改过、不是别人造的、还没过期」这三件事。
//!
//! 与 `token` 模块的分工：那边是随机令牌（库里存哈希、用的时候比对），
//! 这边是把信息直接背在令牌里，服务端不用查库就能读出来。

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{CryptoError, Result};
use crate::MasterKey;

type HmacSha256 = Hmac<Sha256>;

/// 签名。返回 `载荷.签名`，两段都是 base64url（无填充）。
pub fn sign(key: &MasterKey, payload: &str) -> Result<String> {
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let signature = mac(key, &encoded)?;
    Ok(format!("{encoded}.{signature}"))
}

/// 验签并取回载荷。签名不对、格式不对都返回 `Err`。
pub fn verify(key: &MasterKey, token: &str) -> Result<String> {
    let (encoded, signature) = token
        .split_once('.')
        .ok_or_else(|| CryptoError::InvalidToken("签名令牌格式不对".into()))?;
    let expected = mac(key, encoded)?;
    // 定长比较：签名是可以被攻击者逐字节试探的，普通字符串比较会在第一个
    // 不同的字节就返回，时间差足够把签名拼出来
    if !crate::token::constant_time_eq(&expected, signature) {
        return Err(CryptoError::InvalidToken("签名不匹配".into()));
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|err| CryptoError::InvalidToken(format!("载荷不是合法的 base64url：{err}")))?;
    String::from_utf8(raw).map_err(|err| CryptoError::InvalidToken(format!("载荷不是 UTF-8：{err}")))
}

fn mac(key: &MasterKey, message: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(key.bytes())
        .map_err(|err| CryptoError::Cipher(format!("HMAC 初始化失败：{err}")))?;
    mac.update(message.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let key = MasterKey::generate().expect("主密钥");
        let token = sign(&key, r#"{"provider":"github","subject":"42"}"#).expect("签名");
        let payload = verify(&key, &token).expect("验签");
        assert!(payload.contains("\"subject\":\"42\""));

        // 改一个字符就该被拒
        let mut tampered = token.clone();
        tampered.replace_range(0..1, if token.starts_with('e') { "f" } else { "e" });
        assert!(verify(&key, &tampered).is_err());

        // 换一把密钥同样不行
        let other = MasterKey::generate().expect("另一把密钥");
        assert!(verify(&other, &token).is_err());

        assert!(verify(&key, "没有点号").is_err());
    }
}
