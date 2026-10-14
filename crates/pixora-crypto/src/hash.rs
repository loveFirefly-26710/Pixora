//! SHA-256 与 HMAC-SHA1 支持。
//!
//! 服务端在流式接收时边收边算（见上传流程），因此这里同时给出一次性接口
//! 和可增量喂入的 [`Sha256Stream`]，后者可以直接挂在异步读流上。

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// 一次性计算 SHA-256，返回小写十六进制。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// 一次性计算 SHA-256，返回 32 字节摘要。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// 增量 SHA-256。上传大文件时逐块 `update`，不把整文件读进内存。
#[derive(Clone, Default)]
pub struct Sha256Stream {
    inner: Sha256,
    written: u64,
}

impl Sha256Stream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, chunk: &[u8]) {
        self.written += chunk.len() as u64;
        self.inner.update(chunk);
    }

    /// 已经喂入的字节数。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 结束计算并返回小写十六进制摘要。调用后实例不可再用。
    pub fn finalize_hex(self) -> String {
        hex::encode(self.inner.finalize())
    }

    pub fn finalize(self) -> [u8; 32] {
        self.inner.finalize().into()
    }
}

/// HMAC-SHA1，给 TOTP 用（RFC 6238 规定的唯一算法）。
pub fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().into()
}
