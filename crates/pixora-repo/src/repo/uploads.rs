//! 上传相关的仓储方法（预检、blob 登记）。
//!
//! 真正的实现在 `blobs.rs` 里；这里只做再导出，让服务层按业务语义引入
//! （`repo::uploads::NewBlob` 比 `repo::blobs::NewBlob` 读起来更贴近调用点）。

pub use crate::repo::blobs::NewBlob;
