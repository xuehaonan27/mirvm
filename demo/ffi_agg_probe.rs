#![allow(non_snake_case, clippy::missing_transmute_annotations)]
// C1 按值聚合封送合成矩阵探针（open-issues C1 验收面，designs/c1-ffi-agg-design.md §3 片 C）。
// 出向（CallIndirect native_sig 道）：S2 8B pair 档 / S3 24B 含 fn-ptr 成员（TSInput 形）
// 按值参数。入向（thunk 工厂/P1 条目）：mk_pt = Pair 返回重打包、mk_node = 32B sret
// 直传、cb_point = 聚合参数经 P1 条目蹦床（TSInput.read 同形）。两维同构：
// C 源在 driver 内经 Command+cc 现编 .so，双维 dlopen 真 native 依次调用，
// 常量全程自定，无路径/地址/时间输出。
use std::ffi::c_void;
use std::process::Command;

#[repr(C)]
#[derive(Clone, Copy)]
struct S1 {
    a: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct S2 {
    r: u32,
    c: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct S4 {
    a: u64,
    b: u64,
    c: [u32; 4],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct S3 {
    payload: u64,
    cb: extern "C" fn(u64, S2) -> i32,
    enc: u32,
}

extern "C" fn cb_point(payload: u64, pt: S2) -> i32 {
    (payload + pt.r as u64 + pt.c as u64) as i32
}
extern "C" fn mk_pt(x: u32) -> S2 {
    S2 { r: x + 1, c: x + 2 }
}
extern "C" fn mk_node(x: u64) -> S4 {
    S4 {
        a: x + 1,
        b: x + 2,
        c: [x as u32 + 3, 4, 5, 6],
    }
}

const C_SRC: &str = r#"
#include <stdint.h>
typedef struct { uint64_t a; } S1;
typedef struct { uint32_t r, c; } S2;
typedef struct { uint64_t payload; int32_t (*cb)(uint64_t, S2); uint32_t enc; } S3;
typedef struct { uint64_t a, b; uint32_t c[4]; } S4;
uint64_t probe_sum1(S1 s) { return s.a + 7; }
uint64_t probe_sum2(S2 s) { return (uint64_t)s.r * 100 + s.c; }
uint64_t probe_drive(S3 s) { S2 pt = { 5, 7 }; return (uint64_t)s.cb(s.payload, pt) + s.enc; }
typedef S2 (*MkPt)(uint32_t);
uint64_t probe_drive_pt(uint32_t x, MkPt f) { S2 p = f(x); return (uint64_t)p.r * 1000 + p.c; }
typedef S4 (*MkNode)(uint64_t);
uint64_t probe_drive_node(uint64_t x, MkNode f) {
    S4 n = f(x); return n.a + n.b + n.c[0] + n.c[1] + n.c[2] + n.c[3];
}
uint64_t probe_sum4(S4 s) { return s.a + s.b + s.c[0] + s.c[1] + s.c[2] + s.c[3]; }
S2 probe_mk2(uint32_t x) { S2 p = { x + 3, x + 4 }; return p; }
S4 probe_mk4(uint64_t x) { S4 n = { x + 5, x + 6, { 7, 8, 9, 10 } }; return n; }
"#;

unsafe extern "C" {
    fn dlopen(path: *const u8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, sym: *const u8) -> *mut c_void;
    fn dlerror() -> *const u8;
}

fn main() {
    let dir = std::env::temp_dir();
    let base = format!("mirvm-c1-probe-{}", std::process::id());
    let c_path = dir.join(format!("{base}.c"));
    let so_path = dir.join(format!("{base}.so"));
    std::fs::write(&c_path, C_SRC).unwrap();
    let st = Command::new("cc")
        .args(["-O0", "-shared", "-fPIC", "-o"])
        .arg(&so_path)
        .arg(&c_path)
        .status()
        .unwrap();
    assert!(st.success(), "cc 编译探针库失败");

    unsafe {
        let so_c = format!("{}\0", so_path.display());
        let h = dlopen(so_c.as_ptr(), 2);
        if h.is_null() {
            let e = dlerror();
            let m = if e.is_null() { "?" } else { "dlerror" };
            panic!("dlopen 探针库失败: {m}");
        }
        macro_rules! sym {
            ($n:literal) => {{
                let p = dlsym(h, concat!($n, "\0").as_ptr());
                assert!(!p.is_null(), concat!("dlsym ", $n));
                std::mem::transmute::<*mut c_void, _>(p)
            }};
        }
        let p_sum1: extern "C" fn(S1) -> u64 = sym!("probe_sum1");
        let p_sum2: extern "C" fn(S2) -> u64 = sym!("probe_sum2");
        let p_drive: extern "C" fn(S3) -> u64 = sym!("probe_drive");
        let p_drive_pt: extern "C" fn(u32, extern "C" fn(u32) -> S2) -> u64 =
            sym!("probe_drive_pt");
        let p_drive_node: extern "C" fn(u64, extern "C" fn(u64) -> S4) -> u64 =
            sym!("probe_drive_node");

        // 出向：8B 单字段（S1=标量近亲）与 8B pair（S2）
        println!("out sum1={}", p_sum1(S1 { a: 10 })); // 10+7=17
        println!("out sum2={}", p_sum2(S2 { r: 12, c: 5 })); // 12*100+5=1205

        // 出向：24B 聚合内嵌 fn-ptr 成员（TSInput 形）→ native 回调经 P1 条目回解释器
        let s3 = S3 {
            payload: 30,
            cb: cb_point,
            enc: 9,
        };
        println!("cb via agg={}", p_drive(s3)); // cb(30,{5,7})=42; 42+9=51

        // 入向：顶层 fn-ptr 回调返回 8B 聚合（thunk 重打包含义档）
        println!("cb pair-ret={}", p_drive_pt(40, mk_pt)); // (41,42) → 41*1000+42=41042

        // 入向：顶层 fn-ptr 回调返回 32B 聚合（RetAbi::Indirect sret 直传档）
        println!("cb node-ret={}", p_drive_node(10, mk_node)); // 11+12+13+4+5+6=51

        // 出向：32B MEMORY 档按值参数（libffi avalue 读全尺寸字节）
        let p_sum4: extern "C" fn(S4) -> u64 = sym!("probe_sum4");
        let s4 = S4 {
            a: 10,
            b: 11,
            c: [12, 13, 14, 15],
        };
        println!("out sum4={}", p_sum4(s4)); // 10+11+12+13+14+15=75

        // 出向：native 返回 8B 聚合（RetDest::Indirect + ffi memcpy 档）
        let p_mk2: extern "C" fn(u32) -> S2 = sym!("probe_mk2");
        let p = p_mk2(40); // (43, 44)
        println!("out ret2={},{}", p.r, p.c);

        // 出向：native 返回 32B 聚合（sret 档）
        let p_mk4: extern "C" fn(u64) -> S4 = sym!("probe_mk4");
        let n = p_mk4(10); // (15, 16, [7,8,9,10])
        println!(
            "out ret4={}",
            n.a + n.b + n.c.iter().map(|&v| v as u64).sum::<u64>()
        ); // 15+16+7+8+9+10=65

        println!("ffi_agg_probe ok");
    }
}
