#!/usr/bin/env mirvm
---
[dependencies]
bzip2 = "0.6"
---
// bzip2 0.6.1 纯 Rust 后端（默认 feature = libbz2-rs-sys 0.2，C→Rust 忠实翻译，
// 无 build.rs、rust-allocator 下零 libc 依赖）三维差分。压缩/解压主体（块排序
// BWT、MTF、Huffman、CRC）全部以 Rust 形式被解释/JIT——全程无 native 归档。
//
// 路线调研记录（三选一）：
// 1) 【否决】bzip2-rs 0.1.2：纯 Rust 但**仅解码器**（无编码 API），满足不了
//    level 1/5/9 压缩/解压 roundtrip 矩阵。
// 2) 【撞 FRONTIER 绕行】bzip2 = "0.4" C 正路：bzip2-sys 0.1.13 vendored
//    bzip2-1.0.8 以 -DBZ_NO_STDIO 编译 libbz2.a，断言桩 `bz_internal_error`
//    未随归档提供（上游契约：BZ_NO_STDIO 使用者自备），而是定义在 bzip2-sys
//    Rust rlib（`#[no_mangle] pub extern "C" fn bz_internal_error`）。本机无
//    libbz2-dev（pkg-config 探测失败）→ 必然走 vendored 构建。诊断原文
//    （lower 期 panic，exit 101）：
//      Static native library 装载失败: 静态原生归档 `…/libbz2.a` 无法安全转换
//      为共享库（要求 ELF PIC、依赖在本归档内闭合）: /usr/bin/ld:
//      …blocksort.c:111: undefined reference to `bz_internal_error'
//    即 native_archive 闭包检查不予置信的「跨归档符号」（符号在 Rust rlib 而
//    非 .a 内）。native 不受影响（最终链接自然带上 rlib 符号）——手工同款
//    链接行复现确认。任务纪律：跨归档未定义引用报 FRONTIER 别硬绕 → 换路。
// 3) 【采用】bzip2 = "0.6" 默认纯 Rust 后端 libbz2-rs-sys：API 面与 0.4 完全
//    一致（Compression/read/write/bufread/mem/MultiBzDecoder、错误文本相同），
//    无任何原生归档，天然避开该 FRONTIER。本 driver 即「bzip2 的纯 Rust
//    后端」路线。（注：bzip2 0.6 默认后端字节级兼容 C libbz2 的输出，但差分
//    两侧跑同一份 Rust 实现，比较的意义在于解释器 vs native 的语义一致性。）
//
// 覆盖：
// ① read::{BzEncoder,BzDecoder} × level 1/5/9 × {结构化日志, 定种随机, 全零,
//    空} 四档 payload（512B 分块读压缩流 / 777B 分块读回）。
// ② write::{BzEncoder,BzDecoder} 同 level 矩阵 × {struct, random}：333B 分块写、
//    首块后 flush()（BZ_FLUSH 路径）、try_finish + finish、total_in/out；
//    777B 分块写压缩流解码。
// ③ bufread::{BzEncoder,BzDecoder} 小容量 BufReader roundtrip + totals。
// ④ mem 低层面：Compress/Decompress 手喂 Run/Finish 循环（compress_vec /
//    decompress_vec）、small=true 低内存解码、5000B 分喂、wf=3 退化排序
//    fallback 与 wf=250 两档 work_factor、>100KB payload 跨 bzip2 block 边界
//    （level 1 block=100k，220KB → 3 block；level 5 block=500k → 1 block）。
// ⑤ 多流拼接：BzDecoder 只取首流 vs read::MultiBzDecoder 全部；末流后接垃圾的
//    multi 错误路径。
// ⑥ Compression::{new,fast,best,default} level 取值 + try_new 越界优雅拒绝。
// ⑦ 错误路径：垃圾流（magic 缺失）、魔数合法+块体损坏（read 侧一次读穿 /
//    mem 侧手动续喂）、截断流（UnexpectedEof）、write 侧垃圾、mem 侧
//    DataMagic/Data/截断空喂 Result 形态、纯空输入解码、非法 level=0 走
//    Compression::new 越界 panic（0.6 显式校验；静默 hook + catch_unwind）。
//
// 确定性：只打印长度/FNV-1a/布尔/Status 与 ErrorKind 的 Debug、mem::Error 常量
// Display 文本；随机用定种 xorshift64*；无时间/地址/HashMap 序；stderr 为零
// （panic 用例先装静默 hook，driver 零 warning）。
use std::io::{BufReader, Read, Write};

use bzip2::{bufread, read, write, Action, Compress, Compression, Decompress, Status};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定种 xorshift64*（native/mirvm 同序列）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

/// 结构化重复日志（高压缩率；定长序号保证确定性）。`target` 为下限字节数。
fn structured(target: usize) -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < target {
        let line = format!(
            "req {i:05} user=u{:03} action={} cost={}ms status={}\n",
            (i * 13) % 97,
            ["login", "query", "logout"][(i % 3) as usize],
            (i * 7) % 41,
            ["ok", "ok", "fail"][(i % 3) as usize]
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

/// 分块读到 EOF。
fn read_chunked<R: Read>(r: &mut R, n: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; n];
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        out.extend_from_slice(&buf[..k]);
    }
    Ok(out)
}

/// mem 层面一次性压缩：分块 Run 喂入 + Finish 循环到 StreamEnd。
fn mem_oneshot(data: &[u8], level: u32, wf: u32) -> Vec<u8> {
    let mut c = Compress::new(Compression::new(level), wf);
    // bzip2 最坏膨胀远低于该预留；装不下时 Rust 封装会 panic（不可恢复），故笃定预留。
    let mut out = Vec::new();
    out.reserve(data.len() + data.len() / 8 + 65536);
    for ch in data.chunks(1000) {
        let st = c.compress_vec(ch, &mut out, Action::Run).unwrap();
        debug_assert_eq!(st, Status::RunOk);
    }
    loop {
        match c.compress_vec(&[], &mut out, Action::Finish).unwrap() {
            Status::StreamEnd => break,
            _ => {}
        }
    }
    out
}

/// mem 层面解码：`chunk=None` 整喂，否则按块喂；返回（输出， 最后状态）。
fn mem_decode(comp: &[u8], small: bool, chunk: Option<usize>, expect: usize) -> (Vec<u8>, Status) {
    let mut d = Decompress::new(small);
    let mut out = Vec::with_capacity(expect + 16);
    let mut last = Status::Ok;
    match chunk {
        Some(n) => {
            for ch in comp.chunks(n) {
                last = d.decompress_vec(ch, &mut out).unwrap();
            }
        }
        None => {
            last = d.decompress_vec(comp, &mut out).unwrap();
        }
    }
    (out, last)
}

fn main() {
    let sets: Vec<(String, Vec<u8>)> = vec![
        ("struct".to_string(), structured(7168)),
        ("random".to_string(), Rng(0x9E3779B97F4A7C15).bytes(4096)),
        ("zeros".to_string(), vec![0u8; 4096]),
        ("empty".to_string(), Vec::new()),
    ];
    for (name, data) in &sets {
        println!("set {name} raw={} rfnv={:016x}", data.len(), fnv1a(data));
    }

    // ① read::{BzEncoder,BzDecoder} × level 1/5/9 × 四档 payload
    for (name, data) in &sets {
        for level in [1u32, 5, 9] {
            let mut enc = read::BzEncoder::new(&data[..], Compression::new(level));
            let comp = read_chunked(&mut enc, 512).unwrap();
            let mut dec = read::BzDecoder::new(&comp[..]);
            let back = read_chunked(&mut dec, 777).unwrap();
            println!(
                "read {name} L{level} clen={} cfnv={:016x} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ② write::{BzEncoder,BzDecoder} × level 1/5/9 × {struct, random}（中途 flush）
    for (name, data) in &sets[..2] {
        for level in [1u32, 5, 9] {
            let mut enc = write::BzEncoder::new(Vec::new(), Compression::new(level));
            for (i, ch) in data.chunks(333).enumerate() {
                enc.write_all(ch).unwrap();
                if i == 0 {
                    enc.flush().unwrap();
                }
            }
            let tin = enc.total_in();
            let tout_pre = enc.total_out();
            enc.try_finish().unwrap();
            let comp = enc.finish().unwrap();
            let mut dec = write::BzDecoder::new(Vec::new());
            for ch in comp.chunks(777) {
                dec.write_all(ch).unwrap();
            }
            dec.try_finish().unwrap();
            let din = dec.total_in();
            let dout = dec.total_out();
            let back = dec.finish().unwrap();
            println!(
                "write {name} L{level} tin={tin} tout_pre={tout_pre} clen={} cfnv={:016x} din={din} dout={dout} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ③ bufread 通路（1KB 小缓冲）
    let d0 = &sets[0].1;
    let comp_b = {
        let br = BufReader::with_capacity(1024, &d0[..]);
        let mut enc = bufread::BzEncoder::new(br, Compression::new(5));
        let c = read_chunked(&mut enc, 999).unwrap();
        println!(
            "buf-enc tin={} tout={} clen={}",
            enc.total_in(),
            enc.total_out(),
            c.len()
        );
        c
    };
    {
        let br = BufReader::with_capacity(1024, &comp_b[..]);
        let mut dec = bufread::BzDecoder::new(br);
        let back = read_chunked(&mut dec, 999).unwrap();
        println!(
            "buf-dec din={} dout={} cfnv={:016x} rt={}",
            dec.total_in(),
            dec.total_out(),
            fnv1a(&comp_b),
            back == *d0
        );
    }

    // ④ mem 低层面 + work_factor + 跨 block 边界
    let comp_m = mem_oneshot(d0, 5, 30);
    let (back_f, st_f) = mem_decode(&comp_m, false, None, d0.len());
    println!(
        "mem L5 clen={} cfnv={:016x} full-st={st_f:?} full-rt={}",
        comp_m.len(),
        fnv1a(&comp_m),
        back_f == *d0
    );
    let (back_c, st_c) = mem_decode(&comp_m, true, Some(5000), d0.len());
    println!("mem small chunked-st={st_c:?} rt={}", back_c == *d0);
    let zeros = &sets[2].1;
    let comp_z = mem_oneshot(zeros, 1, 3);
    let (back_z, _) = mem_decode(&comp_z, false, None, zeros.len());
    println!(
        "mem zeros L1 wf3 clen={} cfnv={:016x} rt={}",
        comp_z.len(),
        fnv1a(&comp_z),
        back_z == *zeros
    );
    let rnd = &sets[1].1;
    let comp_r = mem_oneshot(rnd, 9, 250);
    let (back_r, _) = mem_decode(&comp_r, false, None, rnd.len());
    println!(
        "mem random L9 wf250 clen={} cfnv={:016x} rt={}",
        comp_r.len(),
        fnv1a(&comp_r),
        back_r == *rnd
    );
    // 跨 block：220KB 结构化（L1 block=100k → 3 block；L5 block=500k → 1 block）
    let big = structured(220_000);
    for level in [1u32, 5] {
        let comp_big = mem_oneshot(&big, level, 30);
        let (back_big, _) = mem_decode(&comp_big, false, Some(65536), big.len());
        println!(
            "mem big L{level} raw={} clen={} cfnv={:016x} rt={}",
            big.len(),
            comp_big.len(),
            fnv1a(&comp_big),
            back_big == big
        );
    }

    // ⑤ 多流拼接：单流解码器只取首流；MultiBzDecoder 取全部
    let c1 = mem_oneshot(d0, 5, 30);
    let c2 = mem_oneshot(&sets[1].1, 5, 30);
    let mut cat = c1.clone();
    cat.extend_from_slice(&c2);
    let first = read_chunked(&mut read::BzDecoder::new(&cat[..]), 777).unwrap();
    println!("multi single n={} first={}", first.len(), first == *d0);
    let mut expect = d0.clone();
    expect.extend_from_slice(&sets[1].1);
    let all = read_chunked(&mut read::MultiBzDecoder::new(&cat[..]), 777).unwrap();
    println!("multi all n={} both={}", all.len(), all == expect);
    let mut tail = c1.clone();
    tail.extend_from_slice(&Rng(0xDEADBEEFCAFEF00D).bytes(64));
    match read_chunked(&mut read::MultiBzDecoder::new(&tail[..]), 777) {
        Ok(v) => println!("multi junk-tail ok n={}", v.len()),
        Err(e) => println!("multi junk-tail err kind={:?} msg={e}", e.kind()),
    }

    // ⑥ Compression 构造面：fast/best/default/new + try_new 优雅拒绝越界
    println!(
        "levels fast={} best={} default={} new5={}",
        Compression::fast().level(),
        Compression::best().level(),
        Compression::default().level(),
        Compression::new(5).level()
    );
    println!(
        "try_new 0={:?} 10={:?} 7={:?}",
        Compression::try_new(0),
        Compression::try_new(10),
        Compression::try_new(7)
    );

    // ⑦ 错误路径
    let junk = Rng(0x0123456789ABCDEF).bytes(64);
    match read_chunked(&mut read::BzDecoder::new(&junk[..]), 777) {
        Ok(v) => println!("read junk ok n={}", v.len()),
        Err(e) => println!("read junk err kind={:?} msg={e}", e.kind()),
    }
    let mut corrupt = c1.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xFF;
    match read_chunked(&mut read::BzDecoder::new(&corrupt[..]), 777) {
        Ok(v) => println!("read corrupt ok n={}", v.len()),
        Err(e) => println!("read corrupt err kind={:?} msg={e}", e.kind()),
    }
    let cut = &c1[..c1.len() * 3 / 5];
    match read_chunked(&mut read::BzDecoder::new(cut), 777) {
        Ok(v) => println!("read truncated ok n={}", v.len()),
        Err(e) => println!("read truncated err kind={:?} msg={e}", e.kind()),
    }
    let mut wd = write::BzDecoder::new(Vec::new());
    match wd.write_all(&junk) {
        Ok(()) => println!("write junk ok out={}", wd.total_out()),
        Err(e) => println!("write junk err kind={:?} msg={e}", e.kind()),
    }
    let mut dm = Decompress::new(false);
    let mut scratch = [0u8; 256];
    match dm.decompress(&junk, &mut scratch) {
        Ok(st) => println!("mem junk st={st:?}"),
        Err(e) => println!("mem junk err: {e}"),
    }
    // mem 块体损坏：全新解码器手动续喂（逐次只喂未消费的尾巴），直至报错/流尽。
    {
        let mut dc = Decompress::new(false);
        let mut outb = vec![0u8; d0.len() + 16];
        let mut off = 0usize;
        loop {
            let before = dc.total_in();
            let res = dc.decompress(&corrupt[off..], &mut outb);
            off += (dc.total_in() - before) as usize;
            match res {
                Ok(Status::StreamEnd) => {
                    println!("mem corrupt st=StreamEnd out={}", dc.total_out());
                    break;
                }
                Ok(_st) if off < corrupt.len() => {}
                Ok(st) => {
                    println!("mem corrupt st={st:?} out={}", dc.total_out());
                    break;
                }
                Err(e) => {
                    println!("mem corrupt err: {e}");
                    break;
                }
            }
        }
    }
    // mem 截断：整块喂入后状态非 StreamEnd；再空喂看库返回值形态。
    let mut dt = Decompress::new(false);
    let mut big_out = vec![0u8; d0.len() + 16];
    let st_t = dt.decompress(cut, &mut big_out).unwrap();
    let eof = dt.decompress(&[], &mut []);
    println!(
        "mem truncated st={st_t:?} out={} eof={eof:?}",
        dt.total_out()
    );
    match read_chunked(&mut read::BzDecoder::new(&[][..]), 777) {
        Ok(v) => println!("read empty ok n={}", v.len()),
        Err(e) => println!("read empty err kind={:?} msg={e}", e.kind()),
    }
    // 非法 level=0：0.6 起 Compression::new 越界 1..=9 即 panic（#[track_caller]
    // const fn；静默 hook 保 stderr 为空 + catch_unwind 断言 panic 发生）。
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| {
        let _ = Compression::new(0);
    });
    println!("invalid-level panic={}", r.is_err());
}
