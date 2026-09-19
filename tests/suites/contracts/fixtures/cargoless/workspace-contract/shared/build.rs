use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let counter = out.join("workspace-build-count");
    let count = fs::read_to_string(&counter)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    fs::write(counter, count.to_string()).unwrap();
    assert_eq!(std::env::var("PROFILE").unwrap(), "debug");
    assert_eq!(std::env::var("OPT_LEVEL").unwrap(), "1");
}
