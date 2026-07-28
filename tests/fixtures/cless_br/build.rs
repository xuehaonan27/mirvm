// D15 P2 切③ 对拍夹具的根 build.rs：读**直接**依赖 bdep 的 links metadata
// （DEP_MYLINKS_FOO——E1(d) 实证：metadata 只给直接依赖者），发 rustc-env
// 与 rustc-cfg（配套 rustc-check-cfg——全局限定：夹具 build.rs 都不发
// warning，byte 对拍先把 warning 面让开）。
fn main() {
    let seen = std::env::var("DEP_MYLINKS_FOO").expect("DEP_MYLINKS_FOO 应在");
    println!("cargo::rustc-env=ROOT_SEEN={seen}");
    println!("cargo::rustc-check-cfg=cfg(root_feat)");
    println!("cargo::rustc-cfg=root_feat");
}
