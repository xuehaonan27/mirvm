#![allow(non_snake_case, clippy::missing_transmute_annotations)]
// Composite matrix probe for by-value aggregate marshalling.
// Outbound (CallIndirect native_sig path): S2 8B pair case / S3 24B with an embedded
// fn-ptr member (TSInput shape) by-value parameter. Inbound (thunk factory / P1 entry):
// mk_pt = pair return repacking, mk_node = 32B sret direct, cb_point = aggregate parameter
// through the P1 entry trampoline (TSInput.read shape). The C source is compiled on the spot
// into a .so and dlopen'd; all constants are self-determined -- no path/address/time output.
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
    assert!(st.success(), "cc failed to compile the probe library");

    unsafe {
        let so_c = format!("{}\0", so_path.display());
        let h = dlopen(so_c.as_ptr(), 2);
        if h.is_null() {
            let e = dlerror();
            let m = if e.is_null() { "?" } else { "dlerror" };
            panic!("dlopen of the probe library failed: {m}");
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

        // Outbound: 8B single field (S1, a scalar next of kin) and 8B pair (S2)
        println!("out sum1={}", p_sum1(S1 { a: 10 })); // 10+7=17
        println!("out sum2={}", p_sum2(S2 { r: 12, c: 5 })); // 12*100+5=1205

        // Outbound: 24B aggregate with an embedded fn-ptr member (TSInput shape) -> native callback returns via the P1 entry
        let s3 = S3 {
            payload: 30,
            cb: cb_point,
            enc: 9,
        };
        println!("cb via agg={}", p_drive(s3)); // cb(30,{5,7})=42; 42+9=51

        // Inbound: top-level fn-ptr callback returning an 8B aggregate (thunk repacking case)
        println!("cb pair-ret={}", p_drive_pt(40, mk_pt)); // (41,42) → 41*1000+42=41042

        // Inbound: top-level fn-ptr callback returning a 32B aggregate (RetAbi::Indirect sret direct case)
        println!("cb node-ret={}", p_drive_node(10, mk_node)); // 11+12+13+4+5+6=51

        // Outbound: 32B MEMORY-case by-value parameter (libffi avalue reads the full-size bytes)
        let p_sum4: extern "C" fn(S4) -> u64 = sym!("probe_sum4");
        let s4 = S4 {
            a: 10,
            b: 11,
            c: [12, 13, 14, 15],
        };
        println!("out sum4={}", p_sum4(s4)); // 10+11+12+13+14+15=75

        // Outbound: native returns an 8B aggregate (RetDest::Indirect + ffi memcpy case)
        let p_mk2: extern "C" fn(u32) -> S2 = sym!("probe_mk2");
        let p = p_mk2(40); // (43, 44)
        println!("out ret2={},{}", p.r, p.c);

        // Outbound: native returns a 32B aggregate (sret case)
        let p_mk4: extern "C" fn(u64) -> S4 = sym!("probe_mk4");
        let n = p_mk4(10); // (15, 16, [7,8,9,10])
        println!(
            "out ret4={}",
            n.a + n.b + n.c.iter().map(|&v| v as u64).sum::<u64>()
        ); // 15+16+7+8+9+10=65

        println!("ffi_agg_probe ok");
    }
}
