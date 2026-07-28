---
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

// D15 P2 切②+③ 全链对拍夹具（tests/diff_cless.sh）：proc-macro2 的 build.rs
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
