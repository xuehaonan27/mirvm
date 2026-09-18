---
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

// differential.cargoless full-chain fixture for proc-macro and build.rs.
// (host compile + run + cfg into its host compile) + serde_core/serde build.rs +
// serde_derive proc-macro host dylib — build.rs and proc-macro mechanisms in the same graph.
// Deterministic output, byte-identical between cargo leg and self leg.
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
