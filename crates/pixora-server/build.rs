//! 让 cargo 盯着前端产物。
//!
//! `rust-embed` 在编译期读 `web/dist`，但它不发 `rerun-if-changed`，
//! 于是「改了前端 → npm run build → cargo build」这条链路里，
//! cargo 认为 crate 没变、不重新编译，嵌入的仍是旧产物——
//! 表现是浏览器里看到的界面永远慢一个版本，而且没有任何报错。
//!
//! 这里显式声明依赖：目录里任何文件变化都触发重编。

fn main() {
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
