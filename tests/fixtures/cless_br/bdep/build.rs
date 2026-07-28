// D15 P2 切③ 对拍夹具的 build-dep：写 $OUT_DIR/gen.rs（OUT_DIR 传播进本
// crate 编译的实证），发 links metadata（foo=bar → 根 build.rs 的
// DEP_MYLINKS_FOO）与 rustc-cfg（配套 check-cfg，零 warning 纪律）。
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR 应在");
    std::fs::write(format!("{out}/gen.rs"), "pub const N: u32 = 7;\n")
        .expect("gen.rs 写入失败");
    println!("cargo::metadata=foo=bar");
    println!("cargo::rustc-check-cfg=cfg(bdep_feat)");
    println!("cargo::rustc-cfg=bdep_feat");
}
