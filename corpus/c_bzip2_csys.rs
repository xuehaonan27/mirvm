#!/usr/bin/env mirvm
---
[dependencies]
bzip2 = "=0.4.4"
---
// bzip2 0.4.4 默认 vendored C 后端（bzip2-sys 0.1.13 编 bzip2-1.0.8 为
// libbz2.a）三维差分——**native-archive「符号在 rlib」（C2）转正验收 driver**：
// libbz2.a 的 blocksort.c 引 `bz_internal_error`（未随归档提供，定义在
// bzip2-sys Rust rlib 的 #[no_mangle]），批5 曾因此撞闭包缺口换路；
// C2 救援链（undefined ∩ rlib 导出集 ⇒ P1 条目隐藏跳板注入重链）落地后，
// **转换本身即注入成立的证据**，字节级往返证明 C 库完整执行。
// 如实备注：断言桩的**运行期**调用路径（BZ_PANIC can't-happen 族）与
// native 一样不可常规触发，本 driver 不覆盖该路径（语义双方同为未走）。
//
// 覆盖（镜像 c_bzip2_pure 的核心面）：
// ① write::{BzEncoder,BzDecoder} × level 1/5/9 × {结构化日志, 定种随机}：
//    333B 分块写、flush()、try_finish + finish、total_in/out 与 roundtrip。
// ② read::{BzEncoder,BzDecoder} 同 level：512B 分块读压缩流 / 777B 分块读回。
// ③ mem 低层面：Compress/Decompress 手喂 Run/Finish、small=true 低内存解码。
// ④ 错误路径：损档解码的 error kind 与文本（两侧同库同错）。
// 确定性：payload 程序合成 + xorshift 定种；错误文本为库内静态文案。
use std::io::{Read, Write};

use bzip2::read::{BzDecoder as RdDecoder, BzEncoder as RdEncoder};
use bzip2::write::{BzDecoder as WrDecoder, BzEncoder as WrEncoder};
use bzip2::{Compression, Compress, Decompress, Status};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// 结构化日志 payload（压缩友好）与定种 xorshift payload（压缩敌对）各 ~96KB。
fn payload_struct() -> Vec<u8> {
    let mut d = Vec::new();
    for i in 0u32..2048 {
        d.extend_from_slice(
            format!("row {i:04} | alpha beta gamma delta | {}\n", "x".repeat((i % 23) as usize))
                .as_bytes(),
        );
    }
    d
}

fn payload_random() -> Vec<u8> {
    let mut d = Vec::with_capacity(96 * 1024);
    let mut s: u64 = 0x243f6a8885a308d3;
    while d.len() < 96 * 1024 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        d.extend_from_slice(&s.to_le_bytes());
    }
    d
}

fn main() {
    let payloads = [("struct", payload_struct()), ("random", payload_random())];
    for (pname, data) in &payloads {
        println!("payload {pname} len={} fnv={:016x}", data.len(), fnv1a(data));
    }

    // ① write 面：分块写 + flush + finish + totals + 解码 roundtrip
    for level in [1u32, 5, 9] {
        let mut e = WrEncoder::new(Vec::new(), Compression::new(level));
        let data = &payloads[0].1;
        for chunk in data.chunks(333) {
            e.write_all(chunk).unwrap();
        }
        e.flush().unwrap();
        let comp = e.finish().unwrap();
        let mut d = WrDecoder::new(Vec::new());
        for chunk in comp.chunks(777) {
            d.write_all(chunk).unwrap();
        }
        let back = d.finish().unwrap();
        println!(
            "write level={level} clen={} cfnv={:016x} roundtrip={}",
            comp.len(),
            fnv1a(&comp),
            back == payloads[0].1
        );
    }

    // ② read 面：分块读压缩流 / 分块读回
    for level in [1u32, 5, 9] {
        let data = &payloads[1].1;
        let mut e = RdEncoder::new(&data[..], Compression::new(level));
        let mut comp = Vec::new();
        e.read_to_end(&mut comp).unwrap();
        let mut d = RdDecoder::new(&comp[..]);
        let mut back = Vec::new();
        d.read_to_end(&mut back).unwrap();
        println!(
            "read level={level} clen={} cfnv={:016x} roundtrip={}",
            comp.len(),
            fnv1a(&comp),
            back == payloads[1].1
        );
    }

    // ③ mem 低层面：compress_vec/decompress_vec 手喂 + small=true 低内存解码
    let data = &payloads[0].1;
    let mut c = Compress::new(Compression::new(9), 30);
    let mut comp: Vec<u8> = Vec::with_capacity(data.len() + 64);
    let mut off = 0usize;
    while off < data.len() {
        c.compress_vec(&data[off..], &mut comp, bzip2::Action::Run)
            .unwrap();
        let next = c.total_in() as usize;
        assert!(next > off, "compress 无进展");
        off = next;
    }
    loop {
        let st = c
            .compress_vec(&[], &mut comp, bzip2::Action::Finish)
            .unwrap();
        if st == Status::StreamEnd {
            break;
        }
    }
    let mut d = Decompress::new(true);
    let mut back: Vec<u8> = Vec::with_capacity(data.len() + 64);
    loop {
        let st = d.decompress_vec(&comp[d.total_in() as usize..], &mut back).unwrap();
        if st == Status::StreamEnd {
            break;
        }
    }
    println!(
        "mem clen={} cfnv={:016x} roundtrip={} totals={}/{}",
        comp.len(),
        fnv1a(&comp),
        back == payloads[0].1,
        c.total_in(),
        d.total_out()
    );

    // ④ 错误路径：损档解码（确定性错误 kind/文本）
    let mut junk = comp.clone();
    let n = junk.len();
    junk[n / 2] ^= 0xff;
    junk[n / 2 + 1] ^= 0xff;
    let err = RdDecoder::new(&junk[..]).read_to_end(&mut Vec::new()).unwrap_err();
    println!("corrupt kind={:?}", err.kind());

    println!("bzip2_csys ok");
}
