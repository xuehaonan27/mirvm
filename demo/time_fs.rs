// 时间 + 文件系统 shims 验收（输出与真实时间/路径无关，可与 native 对拍）
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let t0 = Instant::now();
    let mut acc = 0u64;
    for i in 0..100_000u64 {
        acc = acc.wrapping_add(i * i);
    }
    let el = t0.elapsed();
    println!("elapsed sane: {}", el > Duration::ZERO && el < Duration::from_secs(60));

    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    println!("epoch sane: {}", epoch > 1_700_000_000);

    let p = std::env::temp_dir().join(format!("mirvm_test_{acc}.txt"));
    std::fs::write(&p, "hello mirvm 文件往返").unwrap();
    let s = std::fs::read_to_string(&p).unwrap();
    println!("fs roundtrip: {} (len={})", s == "hello mirvm 文件往返", s.len());
    std::fs::remove_file(&p).unwrap();
    println!("fs cleanup: {}", !p.exists());
}
