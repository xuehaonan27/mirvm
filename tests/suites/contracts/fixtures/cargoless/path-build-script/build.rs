// Root build.rs of the differential fixture: reads the links metadata of its
// **direct** dependency bdep (DEP_MYLINKS_FOO -- metadata reaches direct
// dependents only), then emits rustc-env and rustc-cfg (with the matching
// rustc-check-cfg; fixture build scripts emit no warnings, so the byte-for-byte
// comparison leaves the warning surface alone).
// rerun-if-env-changed: BR_TOGGLE reaches the output through rustc-env, so a
// changed value reruns the root package and changes the guest output, while an unchanged value skips execution and replays the archive byte-for-byte.
fn main() {
    let seen = std::env::var("DEP_MYLINKS_FOO").expect("DEP_MYLINKS_FOO should be set");
    println!("cargo::rustc-env=ROOT_SEEN={seen}");
    println!("cargo::rustc-check-cfg=cfg(root_feat)");
    println!("cargo::rustc-cfg=root_feat");
    let toggle = std::env::var("BR_TOGGLE").unwrap_or_else(|_| "off".into());
    println!("cargo::rustc-env=BR_TOGGLE_SEEN={toggle}");
    println!("cargo::rerun-if-env-changed=BR_TOGGLE");
}
