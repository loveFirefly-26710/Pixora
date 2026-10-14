//! 凭据加密：AES-256-GCM。
//!
//! 存储后端的 Token、SMTP 密码这类凭据在入库前必须过这一层，
//! 主密钥只从环境变量或密钥文件来（见 `pixora_core::config`），不进代码仓库。
//!
//! 密文格式：`v1.<base64(nonce ‖ ciphertext‖tag)>`。
//! 版本前缀留给将来换算法用；没有它，升级算法时无法区分新旧密文。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

use crate::error::{CryptoError, Result};

const VERSION: &str = "v1";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// 主密钥。构造一次，全程共享。
#[derive(Clone)]
pub struct MasterKey {
    key: [u8; KEY_LEN],
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 密钥永远不进日志
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    /// 从 64 位十六进制字符串构造。
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s.trim())
            .map_err(|e| CryptoError::InvalidKey(format!("不是合法的十六进制：{e}")))?;
        Self::from_slice(&bytes)
    }

    /// 从 base64（标准字母表，允许无填充）构造。
    pub fn from_base64(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
            .map_err(|e| CryptoError::InvalidKey(format!("不是合法的 base64：{e}")))?;
        Self::from_slice(&bytes)
    }

    /// 生成一把新的主密钥（安装向导首次启动时用）。
    pub fn generate() -> Result<Self> {
        let mut key = [0u8; KEY_LEN];
        crate::fill_random(&mut key)?;
        Ok(Self { key })
    }

    /// 十六进制表示，用于写入密钥文件。
    pub fn to_hex(&self) -> String {
        hex::encode(self.key)
    }

    /// 自动识别格式：64 个十六进制字符走 hex，其余按 base64 试。
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if trimmed.len() == KEY_LEN * 2 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            Self::from_hex(trimmed)
        } else {
            Self::from_base64(trimmed)
        }
    }

    fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_LEN {
            return Err(CryptoError::InvalidKey(format!(
                "主密钥需要 {KEY_LEN} 字节，实际 {} 字节",
                bytes.len()
            )));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(bytes);
        Ok(Self { key })
    }

    fn cipher(&self) -> Result<Aes256Gcm> {
        Aes256Gcm::new_from_slice(&self.key).map_err(|e| CryptoError::Cipher(e.to_string()))
    }

    /// 加密。每次调用都用新的随机 nonce——GCM 下重复 nonce 会直接泄露明文异或。
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        crate::fill_random(&mut nonce_bytes)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher()?
            .encrypt(nonce, plaintext)
            .map_err(|e| CryptoError::Cipher(e.to_string()))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(format!(
            "{VERSION}.{}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        ))
    }

    pub fn decrypt(&self, encoded: &str) -> Result<Vec<u8>> {
        let (version, payload) = encoded
            .split_once('.')
            .ok_or_else(|| CryptoError::MalformedCiphertext("缺少版本前缀".into()))?;
        if version != VERSION {
            return Err(CryptoError::MalformedCiphertext(format!(
                "不支持的密文版本 {version}"
            )));
        }
        let blob = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| CryptoError::MalformedCiphertext(e.to_string()))?;
        if blob.len() <= NONCE_LEN {
            return Err(CryptoError::MalformedCiphertext("密文长度不足".into()));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        self.cipher()?
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| CryptoError::Cipher("认证失败，密文被篡改或主密钥不匹配".into()))
    }

    pub fn encrypt_str(&self, plaintext: &str) -> Result<String> {
        self.encrypt(plaintext.as_bytes())
    }

    pub fn decrypt_str(&self, encoded: &str) -> Result<String> {
        let bytes = self.decrypt(encoded)?;
        String::from_utf8(bytes).map_err(|e| CryptoError::Cipher(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = MasterKey::generate().unwrap();
        let secret = "smtp-password-复杂口令";
        let encrypted = key.encrypt_str(secret).unwrap();
        assert!(encrypted.starts_with("v1."));
        assert_ne!(encrypted, secret);
        assert_eq!(key.decrypt_str(&encrypted).unwrap(), secret);
    }

    #[test]
    fn nonce_differs_between_calls() {
        let key = MasterKey::generate().unwrap();
        assert_ne!(key.encrypt_str("same").unwrap(), key.encrypt_str("same").unwrap());
    }

    #[test]
    fn wrong_key_fails() {
        let a = MasterKey::generate().unwrap();
        let b = MasterKey::generate().unwrap();
        let encrypted = a.encrypt_str("secret").unwrap();
        assert!(b.decrypt_str(&encrypted).is_err());
    }

    #[test]
    fn parses_hex_and_base64() {
        let key = MasterKey::generate().unwrap();
        assert!(MasterKey::parse(&key.to_hex()).is_ok());
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.key);
        assert!(MasterKey::parse(&b64).is_ok());
        assert!(MasterKey::parse("deadbeef").is_err());
    }
}
