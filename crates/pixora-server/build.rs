//! 让 cargo 盯着前端产物。
//!
//! `rust-embed` 在编译期读 `web/dist`，但它不发 `rerun-if-changed`，
//! 于是「改了前端 → npm run build → cargo build」这条链路里，
//! cargo 认为 crate 没变、不重新编译，嵌入的仍是旧产物——
//! 表现是浏览器里看到的界面永远慢一个版本，而且没有任何报错。
//!
//! 这里显式声明依赖：目录里任何文件变化都触发重编。

fn main() {
    // 只有「产物真的被嵌进二进制」的构建才需要盯着 dist：
    // release 走 rust-embed 的编译期嵌入，dist 变了就必须重编；
    // debug 版是运行时读盘（rust-embed 在没有 debug-embed 时的行为），
    // 盯着它只会让「服务开着的时候改前端」变成 exe 被占用、编译失败。
    if cfg!(debug_assertions) {
        return;
    }
    let dist = std::path::Path::new("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if let Ok(entries) = std::fs::read_dir(dist) {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
            // 子目录（workers/）也要盯上
            if entry.path().is_dir() {
                if let Ok(inner) = std::fs::read_dir(entry.path()) {
                    for file in inner.flatten() {
                        println!("cargo:rerun-if-changed={}", file.path().display());
                    }
                }
            }
        }
    }
}
