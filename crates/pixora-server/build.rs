//! 让 cargo 盯着前端产物。
//!
//! `rust-embed` 在编译期读 `web/dist`，但它不发 `rerun-if-changed`，
//! 于是「改了前端 → npm run build → cargo build」这条链路里，
//! cargo 认为 crate 没变、不重新编译，嵌入的仍是旧产物——
//! 表现是浏览器里看到的界面永远慢一个版本，而且没有任何报错。
//!
//! 这里显式声明依赖：目录里任何文件变化都触发重编。

fn main() {
    // dist 不在仓库里（构建产物不入库），新克隆直接 cargo build 会撞上
    // rust-embed 那句「folder does not exist」，看不出该干什么。这里先给一句人话。
    let embed = std::env::var_os("CARGO_FEATURE_EMBED_WEB").is_some();
    let dist = std::path::Path::new("../../web/dist");
    if embed && !dist.join("index.html").exists() {
        panic!(
            "找不到前端产物 {}：\n\
             先构建前端（cd web && npm ci && npm run build），\n\
             或者加 --no-default-features 编译，运行时用 PIXORA_WEB_DIR 指向源码目录。",
            dist.display()
        );
    }

    // 只有「产物真的被嵌进二进制」的构建才需要盯着 dist：
    // release 走 rust-embed 的编译期嵌入，dist 变了就必须重编；
    // debug 版是运行时读盘（rust-embed 在没有 debug-embed 时的行为），
    // 盯着它只会让「服务开着的时候改前端」变成 exe 被占用、编译失败。
    if cfg!(debug_assertions) {
        return;
    }
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
