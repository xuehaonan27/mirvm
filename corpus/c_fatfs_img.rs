#!/usr/bin/env mirvm
---
[dependencies]
# crates.io 上 fatfs 最新发布即 0.3.6（上游从未发布 0.4）；钉死保证三维同一依赖图。
# default-features=false 摘掉 chrono：默认 TimeProvider 取 chrono::Local::now() 壁钟，
# 是确定性炸弹；改由自定义固定 TimeProvider 锚定全部时间戳（顺带覆盖该 API 面）。
fatfs = { version = "=0.3.6", default-features = false, features = ["std", "alloc"] }
---
// fatfs 0.3.6（纯 Rust FAT 实现）：temp_dir 镜像文件作块设备 → format_volume mkfs
// FAT16（钉 volume_id / volume_label / 4KB 簇）→ 固定时钟挂载 → 建多级目录树
// （8.3 短名 / LFN 长名 / unicode 名，含 unicode 目录套 unicode 文件）→ 写读文件
// （150KB 定种随机大文件跨 ~37 个 4KB 簇、追加、truncate、同目录 rename、跨目录
// move）→ 按名排序递归列树（确定序）→ 删文件/空目录 → 再列树 → 校验剩余结构与
// stats → unmount → boot 扇区 + FAT 首扇区 FNV 锚定 → 删镜像清理（多跑不累加）。
//
// 覆盖 API：format_volume / FormatVolumeOptions / FileSystem::new / fat_type /
// volume_id / read_volume_label_from_root_dir / cluster_size / stats / root_dir /
// create_dir(嵌套路径) / create_file / open_file / open_dir / iter / remove /
// rename(同目录+跨目录) / File 的 Read/Write/Seek/truncate / DirEntry 的 file_name /
// short_file_name / is_dir / is_file / len / attributes / created / modified /
// accessed / 自定义 TimeProvider / unmount。错误路径：开不存在文件、create_dir
// 撞已存在文件、删非空目录、rename 撞已存在目标、open_dir 撞文件。
// 确定性：只打印长度/排序条目/FNV/十六进制/布尔——无路径/壁钟/地址/HashMap 序。
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use fatfs::{
    Date, DateTime, Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek,
    Time, TimeProvider,
};

/// 32MB 镜像：65536 扇区 / 4KB 簇 → ~8k 簇，落在 FAT16 簇数窗口 [4085, 65525)。
const IMG_SIZE: u64 = 32 * 1024 * 1024;
/// 150KB 大文件 ≈ 37 个 4KB 簇，强制跨簇链分配/读回。
const BIG_LEN: usize = 150_000;

/// 固定时钟：created/modified/accessed 全部锚定 2024-03-14 15:09:26.000。
#[derive(Debug)]
struct FixedTime;

impl TimeProvider for FixedTime {
    fn get_current_date(&self) -> Date {
        Date { year: 2024, month: 3, day: 14 }
    }

    fn get_current_date_time(&self) -> DateTime {
        DateTime {
            date: self.get_current_date(),
            time: Time { hour: 15, min: 9, sec: 26, millis: 0 },
        }
    }
}

static FIXED_TIME: FixedTime = FixedTime;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定种 xorshift64* PRNG（native/mirvm 同序列）。
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

fn fmt_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

fn fmt_dt(dt: DateTime) -> String {
    format!(
        "{} {:02}:{:02}:{:02}.{:03}",
        fmt_date(dt.date),
        dt.time.hour,
        dt.time.min,
        dt.time.sec,
        dt.time.millis
    )
}

/// 按 file_name 排序递归列树（确定序）；跳过 "." / ".."。
fn dump_tree<T: ReadWriteSeek>(dir: &Dir<'_, T>, prefix: &str) {
    let mut entries: Vec<_> = dir.iter().map(|r| r.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    let mut n = 0usize;
    for e in &entries {
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        n += 1;
        println!(
            "{}{} {} len={} sfn={:?} attr={:02x} crt={} mod={} acc={}",
            prefix,
            if e.is_dir() { 'd' } else { 'f' },
            name,
            e.len(),
            e.short_file_name(),
            e.attributes().bits(),
            fmt_dt(e.created()),
            fmt_dt(e.modified()),
            fmt_date(e.accessed()),
        );
        if e.is_dir() {
            dump_tree(&e.to_dir(), &format!("{prefix}  "));
        }
    }
    println!("{prefix}(entries = {n})");
}

fn main() {
    // ---- ① 块设备：temp_dir 固定名镜像（先删后建，幂等起点）----
    let img = std::env::temp_dir().join("mirvm_fatfs_img_driver.img");
    let _ = std::fs::remove_file(&img);
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&img)
        .unwrap();
    dev.set_len(IMG_SIZE).unwrap();

    // ---- ② mkfs FAT16（钉卷 ID / 卷标 / 簇大小）----
    dev.seek(SeekFrom::Start(0)).unwrap();
    fatfs::format_volume(
        &mut dev,
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat16)
            .bytes_per_cluster(4096)
            .volume_id(0x1234_ABCD)
            .volume_label(*b"MIRVM TEST "),
    )
    .unwrap();
    dev.seek(SeekFrom::Start(0)).unwrap();

    // ---- ③ 挂载 + 卷信息 ----
    let fs = FileSystem::new(&mut dev, FsOptions::new().time_provider(&FIXED_TIME)).unwrap();
    println!("fat_type = {:?}", fs.fat_type());
    println!("volume_id = {:08x}", fs.volume_id());
    println!("label = {:?}", fs.read_volume_label_from_root_dir().unwrap());
    println!("cluster_size = {}", fs.cluster_size());
    let st = fs.stats().unwrap();
    println!("fresh stats total={} free={}", st.total_clusters(), st.free_clusters());

    // ---- ④ 建目录树（多级 / 短名 / LFN / unicode）----
    let root = fs.root_dir();
    root.create_dir("DOCS").unwrap();
    root.create_dir("PICS").unwrap();
    root.create_dir("long directory 文档").unwrap();
    root.create_dir("DOCS/WORK").unwrap();
    root.create_dir("DOCS/WORK/deep level 3").unwrap();

    // 8.3 短名文件（后面做 truncate）
    let mut readme = Vec::new();
    for i in 0..12u32 {
        readme.extend_from_slice(format!("readme line {i:02} 1234567890 abcdefghij\n").as_bytes());
    }
    {
        let mut f = root.create_file("README.TXT").unwrap();
        f.write_all(&readme).unwrap();
    }
    // LFN 长名文件（后面做 rename/move）
    let lfn_body = "长文件名内容：汉字与 ASCII 混排。\n".as_bytes().to_vec();
    {
        let mut f = root.create_file("my long file name.txt").unwrap();
        f.write_all(&lfn_body).unwrap();
    }
    // unicode 目录里的 unicode 文件
    let uni_body = "ユニコード名の中身\n".as_bytes().to_vec();
    {
        let uni_dir = root.open_dir("long directory 文档").unwrap();
        let mut f = uni_dir.create_file("日本語ファイル.txt").unwrap();
        f.write_all(&uni_body).unwrap();
    }
    // 多级子目录里的 unicode 报告
    let report_body = "季度报告：数据 42，结论 OK。\n".as_bytes().to_vec();
    {
        let docs = root.open_dir("DOCS").unwrap();
        let mut f = docs.create_file("report 2024 数据.txt").unwrap();
        f.write_all(&report_body).unwrap();
    }
    // 跨簇大文件（定种随机）
    let big = Rng(0x9E3779B97F4A7C15).bytes(BIG_LEN);
    let big_fnv = fnv1a(&big);
    {
        let pics = root.open_dir("PICS").unwrap();
        let mut f = pics.create_file("blob.bin").unwrap();
        f.write_all(&big).unwrap();
    }
    // 深层叶文件（结构化日志，后面做追加）
    {
        let deep = root.open_dir("DOCS/WORK/deep level 3").unwrap();
        let mut f = deep.create_file("leaf.log").unwrap();
        for i in 0..40u32 {
            writeln!(f, "line {i:03} pid={} msg=payload{}", (i * 7) % 13, i % 4).unwrap();
        }
    }

    // ---- ⑤ 追加 / truncate / rename ----
    {
        let mut f = root.open_file("DOCS/WORK/deep level 3/leaf.log").unwrap();
        let before = f.seek(SeekFrom::End(0)).unwrap();
        for i in 40..56u32 {
            writeln!(f, "line {i:03} pid={} msg=append{}", (i * 7) % 13, i % 3).unwrap();
        }
        let after = f.seek(SeekFrom::End(0)).unwrap();
        println!("append leaf.log {before} -> {after}");
    }
    {
        let mut f = root.open_file("README.TXT").unwrap();
        f.seek(SeekFrom::Start(200)).unwrap();
        f.truncate().unwrap();
        let end = f.seek(SeekFrom::End(0)).unwrap();
        println!("truncate README.TXT -> {end}");
    }
    // 同目录 rename + 跨目录 move
    let docs_dir = root.open_dir("DOCS").unwrap();
    root.rename("my long file name.txt", &root, "renamed final.txt").unwrap();
    root.rename("renamed final.txt", &docs_dir, "moved final.txt").unwrap();
    println!("rename + move ok");

    // ---- ⑥ 列树（建后）----
    println!("-- tree after build --");
    dump_tree(&root, "");

    // ---- ⑦ 读回校验 ----
    let mut buf = Vec::new();
    root.open_file("PICS/blob.bin").unwrap().read_to_end(&mut buf).unwrap();
    println!(
        "blob.bin len={} fnv={:016x} ok={}",
        buf.len(),
        fnv1a(&buf),
        buf == big && fnv1a(&buf) == big_fnv
    );
    buf.clear();
    root.open_file("README.TXT").unwrap().read_to_end(&mut buf).unwrap();
    println!(
        "README.TXT len={} fnv={:016x} trunc-ok={}",
        buf.len(),
        fnv1a(&buf),
        buf == readme[..200]
    );
    buf.clear();
    root.open_file("DOCS/WORK/deep level 3/leaf.log").unwrap().read_to_end(&mut buf).unwrap();
    let leaf = String::from_utf8(buf).unwrap();
    println!("leaf.log lines={} last={:?}", leaf.lines().count(), leaf.lines().last().unwrap());
    buf = Vec::new();
    root.open_file("DOCS/moved final.txt").unwrap().read_to_end(&mut buf).unwrap();
    println!("moved final.txt len={} ok={}", buf.len(), buf == lfn_body);
    buf.clear();
    root.open_file("long directory 文档/日本語ファイル.txt").unwrap().read_to_end(&mut buf).unwrap();
    println!("unicode file len={} ok={}", buf.len(), buf == uni_body);

    // ---- ⑧ 错误路径（消息为 crate 固定串，确定）----
    match root.open_file("NOPE.TXT") {
        Ok(_) => println!("open-missing: unexpected ok"),
        Err(e) => println!("open-missing err kind={:?} msg={e}", e.kind()),
    }
    match root.create_dir("README.TXT") {
        Ok(_) => println!("mkdir-over-file: unexpected ok"),
        Err(e) => println!("mkdir-over-file err kind={:?} msg={e}", e.kind()),
    }
    match root.remove("DOCS") {
        Ok(_) => println!("rmdir-nonempty: unexpected ok"),
        Err(e) => println!("rmdir-nonempty err kind={:?} msg={e}", e.kind()),
    }
    match root.rename("README.TXT", &docs_dir, "report 2024 数据.txt") {
        Ok(_) => println!("rename-over-existing: unexpected ok"),
        Err(e) => println!("rename-over-existing err kind={:?} msg={e}", e.kind()),
    }
    match root.open_dir("README.TXT") {
        Ok(_) => println!("opendir-on-file: unexpected ok"),
        Err(e) => println!("opendir-on-file err kind={:?} msg={e}", e.kind()),
    }

    // ---- ⑨ 删除 + 再列 + stats ----
    root.remove("DOCS/WORK/deep level 3/leaf.log").unwrap();
    root.remove("DOCS/WORK/deep level 3").unwrap(); // 已空
    root.remove("PICS/blob.bin").unwrap();
    root.remove("PICS").unwrap();
    println!("-- tree after delete --");
    dump_tree(&root, "");
    let st2 = fs.stats().unwrap();
    println!("final stats total={} free={}", st2.total_clusters(), st2.free_clusters());

    // ---- ⑩ unmount + 镜像锚定 + 清理 ----
    // Dir 带 drop glue，显式drop 释放对 fs/dev 的借用，才能 move fs 进 unmount。
    drop(docs_dir);
    drop(root);
    fs.unmount().unwrap();
    dev.seek(SeekFrom::Start(0)).unwrap();
    let mut boot = [0u8; 512];
    dev.read_exact(&mut boot).unwrap();
    println!("boot fnv={:016x}", fnv1a(&boot));
    let fat_start = u16::from_le_bytes([boot[14], boot[15]]) as u64 * 512;
    dev.seek(SeekFrom::Start(fat_start)).unwrap();
    let mut fat0 = [0u8; 512];
    dev.read_exact(&mut fat0).unwrap();
    println!("fat0 fnv={:016x}", fnv1a(&fat0));
    println!("image len = {}", dev.metadata().unwrap().len());
    drop(dev);
    std::fs::remove_file(&img).unwrap();
    println!("cleanup done");
}
