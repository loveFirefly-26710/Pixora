//! Base32（RFC 4648），给 TOTP 密钥用。
//!
//! 编码不带填充（`=`），这是各家验证器 App 的通行做法；
//! 解码时反过来要宽容：接受小写、空格、缺失或多余的填充。

use crate::error::{CryptoError, Result};

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// 编码为不带填充的大写 Base32。
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buffer: u64 = 0;
    let mut bits = 0u32;

    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits += 8;
        while bits >= 5 {
            let index = ((buffer >> (bits - 5)) & 0x1f) as usize;
            out.push(ALPHABET[index] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

/// 解码。容忍小写、空格与填充符。
pub fn decode(input: &str) -> Result<Vec<u8>> {
    let mut buffer: u64 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 5 / 8);

    for ch in input.chars() {
        let value = match ch {
            'A'..='Z' => ch as u8 - b'A',
            'a'..='z' => ch as u8 - b'a',
            '2'..='7' => ch as u8 - b'2' + 26,
            '=' | ' ' | '\t' | '\n' | '\r' | '-' => continue,
            other => {
                return Err(CryptoError::Base32(format!("非法字符 {other:?}")));
            }
        };
        buffer = (buffer << 5) | value as u64;
        bits += 5;
        if bits >= 8 {
            out.push(((buffer >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        // RFC 4648 §10 的测试向量（去掉填充）
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "MY");
        assert_eq!(encode(b"fo"), "MZXQ");
        assert_eq!(encode(b"foo"), "MZXW6");
        assert_eq!(encode(b"foob"), "MZXW6YQ");
        assert_eq!(encode(b"fooba"), "MZXW6YTB");
        assert_eq!(encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn decode_is_forgiving() {
        assert_eq!(decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(decode("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(decode("MZXW 6YTB OI==").unwrap(), b"foobar");
        assert!(decode("MZXW1").is_err());
    }

    #[test]
    fn roundtrip_random_lengths() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(decode(&encode(&data)).unwrap(), data);
        }
    }
}
