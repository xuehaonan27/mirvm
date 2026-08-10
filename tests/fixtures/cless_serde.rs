---
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

// differential.cargoless 的 proc-macro 与 build.rs 全链夹具。
// （host 编译 + 执行 + cfg 进其 host 编译）+ serde_core/serde 的 build.rs +
// serde_derive proc-macro host dylib——build.rs 与 proc-macro 两条机制同图。
// 输出确定性文本，cargo 腿与 self 腿逐字节对拍。
fn main() {
    #[derive(serde::Serialize)]
    struct Point {
        x: u32,
        y: u32,
    }
    let s = serde_json::to_string(&Point { x: 3, y: 4 }).unwrap();
    println!("cless-serde {s}");
    std::process::exit(8);
}
