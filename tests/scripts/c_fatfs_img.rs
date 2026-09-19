#!/usr/bin/env mirvm
---
[dependencies]
# fatfs's newest crates.io release is 0.3.6 (0.4 was never published), so pinning it
# keeps all three dimensions on one dependency graph. default-features=false drops
# chrono (the default TimeProvider uses the wall clock) for a fixed-time provider.
fatfs = { version = "=0.3.6", default-features = false, features = ["std", "alloc"] }
---
// fatfs 0.3.6 (pure-Rust FAT): a temp_dir image file as the block device -> format_volume
// mkfs FAT16 (pinned volume_id / volume_label / 4KB clusters) -> mount with a fixed clock
// -> build a multi-level tree (8.3 short names / LFN long names / unicode names) -> write
// and read files (a 150KB seeded file spanning ~37 clusters, append, truncate, rename,
// cross-dir move) -> list recursively by name -> delete -> list again -> unmount -> anchor
// boot + first FAT sector FNV -> delete the image (repeatable).
//
// API coverage: format_volume / FormatVolumeOptions / FileSystem::new / fat_type /
// volume_id / read_volume_label_from_root_dir / cluster_size / stats / root_dir /
// create_dir (nested) / create_file / open_file / open_dir / iter / remove / rename /
// File Read/Write/Seek/truncate / DirEntry name accessors and metadata / the custom
// TimeProvider / unmount. Error paths: missing file, create_dir over a file, removing a
// non-empty dir, rename onto an existing target, and open_dir on a file.
// Determinism: only lengths/sorted entries/FNV/hex/bools are printed -- no paths, wall
// clock, addresses or HashMap order.
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use fatfs::{
    Date, DateTime, Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek,
    Time, TimeProvider,
};

/// 32MB image: 65536 sectors / 4KB clusters -> ~8k clusters, inside the FAT16 window [4085, 65525).
const IMG_SIZE: u64 = 32 * 1024 * 1024;
/// A 150KB file is ~37 4KB clusters, forcing cross-cluster chain allocation and readback.
const BIG_LEN: usize = 150_000;

/// Fixed clock: created/modified/accessed are all anchored to 2024-03-14 15:09:26.000.
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

/// Seeded xorshift64* PRNG (same sequence on native and mirvm).
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

/// Recursively list the tree sorted by file_name (deterministic order); skips "." / "..".
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
    // ---- ① block device: fixed-name temp_dir image (delete then create, idempotent start) ----
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

    // ---- ② mkfs FAT16 (pinned volume ID / label / cluster size) ----
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

    // ---- ③ mount + volume info ----
    let fs = FileSystem::new(&mut dev, FsOptions::new().time_provider(&FIXED_TIME)).unwrap();
    println!("fat_type = {:?}", fs.fat_type());
    println!("volume_id = {:08x}", fs.volume_id());
    println!("label = {:?}", fs.read_volume_label_from_root_dir().unwrap());
    println!("cluster_size = {}", fs.cluster_size());
    let st = fs.stats().unwrap();
    println!("fresh stats total={} free={}", st.total_clusters(), st.free_clusters());

    // ---- ④ build the directory tree (multi-level / short names / LFN / unicode) ----
    let root = fs.root_dir();
    root.create_dir("DOCS").unwrap();
    root.create_dir("PICS").unwrap();
    root.create_dir("long directory 文档").unwrap();
    root.create_dir("DOCS/WORK").unwrap();
    root.create_dir("DOCS/WORK/deep level 3").unwrap();

    // 8.3 short-name file (truncated later)
    let mut readme = Vec::new();
    for i in 0..12u32 {
        readme.extend_from_slice(format!("readme line {i:02} 1234567890 abcdefghij\n").as_bytes());
    }
    {
        let mut f = root.create_file("README.TXT").unwrap();
        f.write_all(&readme).unwrap();
    }
    // LFN long-name file (renamed/moved later)
    let lfn_body = "长文件名内容：汉字与 ASCII 混排。\n".as_bytes().to_vec();
    {
        let mut f = root.create_file("my long file name.txt").unwrap();
        f.write_all(&lfn_body).unwrap();
    }
    // unicode file inside a unicode directory
    let uni_body = "ユニコード名の中身\n".as_bytes().to_vec();
    {
        let uni_dir = root.open_dir("long directory 文档").unwrap();
        let mut f = uni_dir.create_file("日本語ファイル.txt").unwrap();
        f.write_all(&uni_body).unwrap();
    }
    // unicode report inside a multi-level subdirectory
    let report_body = "季度报告：数据 42，结论 OK。\n".as_bytes().to_vec();
    {
        let docs = root.open_dir("DOCS").unwrap();
        let mut f = docs.create_file("report 2024 数据.txt").unwrap();
        f.write_all(&report_body).unwrap();
    }
    // cross-cluster large file (seeded random)
    let big = Rng(0x9E3779B97F4A7C15).bytes(BIG_LEN);
    let big_fnv = fnv1a(&big);
    {
        let pics = root.open_dir("PICS").unwrap();
        let mut f = pics.create_file("blob.bin").unwrap();
        f.write_all(&big).unwrap();
    }
    // deep leaf file (structured log, appended later)
    {
        let deep = root.open_dir("DOCS/WORK/deep level 3").unwrap();
        let mut f = deep.create_file("leaf.log").unwrap();
        for i in 0..40u32 {
            writeln!(f, "line {i:03} pid={} msg=payload{}", (i * 7) % 13, i % 4).unwrap();
        }
    }

    // ---- ⑤ append / truncate / rename ----
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
    // same-directory rename + cross-directory move
    let docs_dir = root.open_dir("DOCS").unwrap();
    root.rename("my long file name.txt", &root, "renamed final.txt").unwrap();
    root.rename("renamed final.txt", &docs_dir, "moved final.txt").unwrap();
    println!("rename + move ok");

    // ---- ⑥ list the tree (after build) ----
    println!("-- tree after build --");
    dump_tree(&root, "");

    // ---- ⑦ read-back verification ----
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

    // ---- ⑧ error paths (the messages are the crate's fixed strings, deterministic) ----
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

    // ---- ⑨ delete + list again + stats ----
    root.remove("DOCS/WORK/deep level 3/leaf.log").unwrap();
    root.remove("DOCS/WORK/deep level 3").unwrap(); // now empty
    root.remove("PICS/blob.bin").unwrap();
    root.remove("PICS").unwrap();
    println!("-- tree after delete --");
    dump_tree(&root, "");
    let st2 = fs.stats().unwrap();
    println!("final stats total={} free={}", st2.total_clusters(), st2.free_clusters());

    // ---- ⑩ unmount + image anchors + cleanup ----
    // Dir carries drop glue; an explicit drop releases the borrow of fs/dev so fs can move into unmount.
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
