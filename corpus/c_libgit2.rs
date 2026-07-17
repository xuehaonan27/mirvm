#!/usr/bin/env mirvm
---
[dependencies]
git2 = { version = "=0.20.4", default-features = false }
libgit2-sys = "=0.18.5"
---
// git2 0.20.4（libgit2 C FFI；任务钉 0.19/0.20 stable 线最新 = 0.20.4——上游
// 已发 0.21.0（2026-07-17），不在本槽位授权范围）+ libgit2-sys 钉 =0.18.5
// （内嵌 C 树 1.9.4；精确钉死 C 源码面，防 ^0.18.3 漂到新 C 版）。feature：
// default-features = false——本面全本地（init/写文件/commit/tree/log/status/
// refs），不需要网络；剥掉 default（ssh+https）拉入的 libssh2-sys/openssl-sys/
// openssl-probe 三重 C 构建（corpus feature 最小化纪律）。宿主无系统 libgit2
// （pkg-config 探不到 libgit2.pc）→ libgit2-sys 走 vendored 内置静态构建（cc
// 把 bundled libgit2 1.9.4 编成 .a；mirvm 经「static .a → .so 闭包」
// native-archive 通道加载，批3 rusqlite / 批5 zstd 已实证）——重 C 构建属
// 预期（任务明示）。
//
// 测试面（批7 波2；全程 git2 API，不 spawn git 命令）：
//   libgit2 运行时版本锚（1.9.4）→ 临时目录 init_opts(initial_head="master"，
//   钉死分支名不吃全局 gitconfig）→ std::fs 写 3 个固定文件（README.md /
//   src/main.rs / 定种 xorshift data.bin 32B）→ index.add_path（排序序）+
//   write + write_tree → 显式固定签名（名字/邮箱/时间戳全硬编码，Time::new
//   恒定 offset +01:00，不吃 env）两次 commit（第二次改 README + 加
//   src/lib.rs，父=c1）→ annotated tag v1.0（固定 tagger）→ 读回全打印：
//   两个 commit id / tree id（纯内容寻址，sha1 锚定）、HEAD name/target、
//   revwalk TIME|TOPOLOGICAL 序 log（id/time/offset/parents/summary）、tree
//   递归清单（{:06o} mode/kind/path/oid，git 规范序）、get_path 命中/未命中、
//   5 blob len+fnv1a+与原文逐字节比对 bool、refs 列表（BTreeMap 序）、本地
//   分支列表、HEAD/master reflog 条数+逐条（old→new/msg）、status 空检查
//   （→制造脏态：改 README + 两个 untracked（含嵌套目录）打印排序后
//   path/flags → 复原后再空检查）、index 条目（mode/path/oid，BTree 序）、
//   三个错误路径（缺引用 / 零 oid tag / open 非仓库——打印 ErrorCode/
//   ErrorClass 枚举 Debug，不打印可能含路径的 message）、workdir 文件清单
//   （递归排序，跳过 .git）、结尾清理目录。
//
// 确定性：全部身份/时间戳/内容硬编码；commit/tree/tag 走 sha1 内容寻址；
// 集合一律 BTreeMap/BTreeSet；无 HashMap 序/RNG（xorshift 定种）/时间/线程/
// 绝对路径/env 入输出；B 维 native 实测 stdout 58 行全确定（重跑 commit id
// 锚定同值）、stderr 真空、exit 0。
//
// ★ FRONTIER（2026-07-17 锁定 expected-red；A 维 lower 期响亮失败，B 维
//   native 三维对照 oracle 全绿——纯引擎缺口，driver 层无合法绕行）：
//   trap 原文（mirvm run，exit 101，rustc thread panic）：
//     src/lower/mod.rs:2002: Static native library 装载失败: 静态原生归档
//     `…/build/libgit2-sys-*/out/build/libgit2.a` 无法安全转换为共享库
//     （要求 ELF PIC、依赖在本归档内闭合）:
//     /usr/bin/ld: … indexer.c:361: undefined reference to `crc32'
//     … filebuf.c/zstream.c: undefined reference to `deflate'/`inflate' 族
//   根因（机制级实锤）：native_archive.rs 的闭包链接行 = LINK_PREFIX
//   （-shared -z,defs --whole-archive）+ 归档 + LINK_SUFFIX（行 21 写死
//   -lm -ldl -lpthread -lrt -lutil -lgcc_s）。libz-sys 0.18.x stock-zlib
//   动态模式只经 rlib 元数据传播 `cargo:rustc-link-lib=z`（脚本 target 的
//   build output 实证，无 libz.a 产出）→ 批5 修复（lower/mod.rs:2040-2102）
//   收集 crate 图动态元数据库做 RTLD_GLOBAL 预载 + 移交 module.native_libs，
//   但闭包链接是**独立 cc 子进程**且行序上发生在预载之前（2001 < 2093）——
//   预载够不着；独立 ld 的 -z defs 必须命令行自带 -lz。缺的不是 glibc 家族
//   而是 crate 图传播的动态元数据库，LINK_SUFFIX 硬编码清单覆盖不到。
//   与批3 rusqlite（libsqlite3.a 引 libm `log`）同族新形态——那次修的是
//   std/#link 恒给清单，这次要的是批5 收集的 dylib_names 移交闭包链接行
//   （-l<name> → 产 DT_NEEDED 由宿主解析；缓存键 link_flags 需同纳名单）。
//   证据链：①手动重放同链接行（无 -lz）复现 17 处 undefined reference
//   （crc32/deflate*/inflate*）；②同命令追加 -lz → LINK_OK 且产
//   DT_NEEDED libz.so.1；③最小复现 /tmp/mirvm_probe_libgit2sys_only.rs
//   （依赖仅 libgit2-sys = "=0.18.5"、单 FFI 调用、无 git2 绑定层）同址
//   同文案复现——与 git2 Rust 层无关。
//   绕行排查（均不可）：静态 libz（LIBZ_SYS_STATIC=1 或 zlib-ng-compat
//   feature）→ zlib 变独立静态归档，闭包逐归档独立 -z defs 闭合，跨归档
//   引用同病；build 期 env 不可记入三维纪律；LIBGIT2_SYS_USE_PKG_CONFIG
//   → 宿主无 libgit2.pc，且偏离任务指定的「内置静态构建」面。
//   修复后断言：本 driver 无需改动即应三维全绿（B 维输出即 oracle）。
//   另注：刻意未注册任何 Rust→C 回调（绕开批3 记档的「结构体内嵌 fn-ptr
//   回调」thunk 盲区；TreeWalk 回调 API 不用，用 tree.iter() 等价替代）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_libgit2.rs
//   B: cd "$(grep -l 'name = "c_libgit2"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_libgit2.rs
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use git2::{
    ObjectType, Oid, Repository, RepositoryInitOptions, Signature, Sort, StatusOptions, Time,
};

const NAME: &str = "Mirvm Tester";
const EMAIL: &str = "tester@example.com";
/// 两个 commit 的固定时间戳（秒；offset 恒 +01:00 = 60 分钟）。
const T1: i64 = 1_700_000_000;
const T2: i64 = 1_700_000_600;

const README1: &[u8] = b"# mirvm libgit2\nC-FFI differential sample.\n";
const README2: &[u8] =
    b"# mirvm libgit2\nC-FFI differential sample.\nsecond commit updates this file.\n";
const MAIN_RS: &[u8] = b"fn main() {\n    println!(\"libgit2\");\n}\n";
const LIB_RS: &[u8] = b"pub fn answer() -> u32 { 42 }\n";
const MSG1: &str = "initial commit: add base files\n\nREADME.md, src/main.rs, data.bin\n";
const MSG2: &str = "second commit: update README, add lib.rs\n";

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定种 xorshift64* 生成 32 字节二进制 blob（二进制 blob 边界）。
fn data_bin() -> Vec<u8> {
    let mut out = Vec::new();
    let mut x = 0x9E3779B97F4A7C15u64;
    while out.len() < 32 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
    }
    out.truncate(32);
    out
}

fn sig(secs: i64) -> Signature<'static> {
    Signature::new(NAME, EMAIL, &Time::new(secs, 60)).unwrap()
}

/// 递归收集 tree 遍历行（顺序 = tree 条目序，git 规范序 → 确定）。
fn walk_tree(repo: &Repository, tree: &git2::Tree, prefix: &str, out: &mut Vec<String>) {
    for e in tree.iter() {
        let path = format!("{prefix}{}", e.name().unwrap());
        out.push(format!(
            "{:06o} {:?} {} {}",
            e.filemode(),
            e.kind().unwrap(),
            path,
            e.id()
        ));
        if e.kind() == Some(ObjectType::Tree) {
            let sub = repo.find_tree(e.id()).unwrap();
            walk_tree(repo, &sub, &format!("{path}/"), out);
        }
    }
}

/// 递归列出 workdir 文件（跳过 .git），BTreeSet 序保证确定。
fn list_workdir(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let rel = format!("{prefix}{name}");
        let p = entry.path();
        if p.is_dir() {
            list_workdir(&p, &format!("{rel}/"), out);
        } else {
            out.insert(rel);
        }
    }
}

fn main() {
    let v = git2::Version::get();
    let (maj, min, rev) = v.libgit2_version();
    println!("libgit2 version = {maj}.{min}.{rev}");
    println!(
        "libgit2 features vendored={} threads={} https={} ssh={} nsec={}",
        v.vendored(),
        v.threads(),
        v.https(),
        v.ssh(),
        v.nsec()
    );

    // 固定子目录：开头清一次再建，结尾删除——多跑不累加。
    let root = std::env::temp_dir().join("mirvm_corpus_libgit2");
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();

    // git2 对象（Tree/Commit/Statuses/…）借用 repo 且实现 Drop——借用区随
    // 内层作用域结束自动按逆序释放（repo 最先声明故最后 drop）；作用域外再
    // 做目录清理。
    {
        // ---- ① init + 写文件 + 两个固定时间戳的 commit ----
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("master");
        let repo = Repository::init_opts(&root, &opts).unwrap();
        println!(
            "init bare={} empty={} detached={} state={:?}",
            repo.is_bare(),
            repo.is_empty().unwrap(),
            repo.head_detached().unwrap(),
            repo.state()
        );

        let data = data_bin();
        fs::create_dir_all(root.join("src")).unwrap();
        for (p, c) in [
            ("README.md", README1),
            ("src/main.rs", MAIN_RS),
            ("data.bin", data.as_slice()),
        ] {
            fs::write(root.join(p), c).unwrap();
        }

        let mut index = repo.index().unwrap();
        // 排序序 add，消除入参序影响。
        for p in ["README.md", "data.bin", "src/main.rs"] {
            index.add_path(Path::new(p)).unwrap();
        }
        index.write().unwrap();
        let tree1 = index.write_tree().unwrap();
        let sig1 = sig(T1);
        let tree1_obj = repo.find_tree(tree1).unwrap();
        let c1 = repo
            .commit(Some("HEAD"), &sig1, &sig1, MSG1, &tree1_obj, &[])
            .unwrap();
        println!("commit1 = {c1}");
        println!("tree1 = {tree1}");

        // 第二个 commit：改 README.md，加 src/lib.rs（workdir 同步更新）。
        fs::write(root.join("README.md"), README2).unwrap();
        fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();
        for p in ["README.md", "src/lib.rs"] {
            index.add_path(Path::new(p)).unwrap();
        }
        index.write().unwrap();
        let tree2 = index.write_tree().unwrap();
        let sig2 = sig(T2);
        let tree2_obj = repo.find_tree(tree2).unwrap();
        let c1_obj = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("HEAD"), &sig2, &sig2, MSG2, &tree2_obj, &[&c1_obj])
            .unwrap();
        println!("commit2 = {c2}");
        println!("tree2 = {tree2}");
        assert_ne!(c1, c2);
        assert_ne!(tree1, tree2);

        // annotated tag（固定 tagger）充实 refs 列表。
        let tag_oid = repo
            .tag("v1.0", c1_obj.as_object(), &sig1, "release v1.0\n", false)
            .unwrap();
        let tag = repo.find_tag(tag_oid).unwrap();
        println!("tag {} = {}", tag.name().unwrap(), tag_oid);
        println!(
            "tag peeled == commit1 = {}",
            tag.target().unwrap().id() == c1
        );

        // ---- ② HEAD / log ----
        let head = repo.head().unwrap();
        println!("head name = {}", head.name().unwrap());
        println!("head shorthand = {}", head.shorthand().unwrap());
        println!("head id = {}", head.target().unwrap());
        println!(
            "head state = {:?} empty={}",
            repo.state(),
            repo.is_empty().unwrap()
        );

        let mut rw = repo.revwalk().unwrap();
        rw.set_sorting(Sort::TIME | Sort::TOPOLOGICAL).unwrap();
        rw.push_head().unwrap();
        let mut nlog = 0;
        for r in &mut rw {
            let c = repo.find_commit(r.unwrap()).unwrap();
            println!(
                "log {} time={} off={} parents={} {}",
                c.id(),
                c.time().seconds(),
                c.time().offset_minutes(),
                c.parent_count(),
                c.summary().unwrap()
            );
            nlog += 1;
        }
        println!("log count = {nlog}");

        // ---- ③ tree 遍历 + 路径查找 ----
        let mut rows = Vec::new();
        walk_tree(&repo, &tree2_obj, "", &mut rows);
        println!("tree2 entries = {}", rows.len());
        for r in &rows {
            println!("tree {r}");
        }
        let e = tree2_obj.get_path(Path::new("src/main.rs")).unwrap();
        println!("lookup src/main.rs = {:06o} {}", e.filemode(), e.id());
        match tree2_obj.get_path(Path::new("nope.txt")) {
            Ok(_) => println!("lookup nope.txt unexpected ok"),
            Err(e) => println!("lookup nope.txt err = {:?}/{:?}", e.code(), e.class()),
        }

        // ---- ④ blob 读取校验（len/fnv/逐字节比对原文）----
        let b_readme1 = tree1_obj.get_path(Path::new("README.md")).unwrap().id();
        let b_readme2 = tree2_obj.get_path(Path::new("README.md")).unwrap().id();
        let b_main = tree2_obj.get_path(Path::new("src/main.rs")).unwrap().id();
        let b_lib = tree2_obj.get_path(Path::new("src/lib.rs")).unwrap().id();
        let b_data_t1 = tree1_obj.get_path(Path::new("data.bin")).unwrap().id();
        let b_data_t2 = tree2_obj.get_path(Path::new("data.bin")).unwrap().id();
        println!("blob dedup data t1==t2 = {}", b_data_t1 == b_data_t2);
        for (label, oid, expect) in [
            ("readme1", b_readme1, README1),
            ("readme2", b_readme2, README2),
            ("main", b_main, MAIN_RS),
            ("lib", b_lib, LIB_RS),
            ("data", b_data_t2, data.as_slice()),
        ] {
            let blob = repo.find_blob(oid).unwrap();
            let content = blob.content();
            println!(
                "blob {label} len={} fnv={:016x} ok={}",
                content.len(),
                fnv1a(content),
                content == expect
            );
            assert_eq!(content, expect);
        }

        // ---- ⑤ refs 列表（BTreeMap 序）+ 本地分支 ----
        let mut refs = BTreeMap::new();
        for r in repo.references().unwrap() {
            let r = r.unwrap();
            let target = match r.target() {
                Some(oid) => oid.to_string(),
                None => format!("-> {}", r.symbolic_target().unwrap()),
            };
            refs.insert(r.name().unwrap().to_string(), target);
        }
        println!("refs count = {}", refs.len());
        for (name, target) in &refs {
            println!("ref {name} = {target}");
        }
        let mut branches = Vec::new();
        for b in repo.branches(Some(git2::BranchType::Local)).unwrap() {
            branches.push(b.unwrap().0.name().unwrap().unwrap().to_string());
        }
        branches.sort();
        println!("local branches = {branches:?}");

        // ---- ⑥ HEAD / master reflog ----
        for name in ["HEAD", "refs/heads/master"] {
            let rl = repo.reflog(name).unwrap();
            let mut n = 0;
            for entry in rl.iter() {
                println!(
                    "reflog {name} {} -> {} msg={}",
                    entry.id_old(),
                    entry.id_new(),
                    entry.message().unwrap_or("")
                );
                n += 1;
            }
            println!("reflog {name} count = {n}");
        }

        // ---- ⑦ status：净 → 脏 → 复原净 ----
        let mut so = StatusOptions::new();
        so.include_untracked(true).recurse_untracked_dirs(true);
        let sts = repo.statuses(Some(&mut so)).unwrap();
        println!("status clean entries = {}", sts.len());
        assert_eq!(sts.len(), 0);

        fs::write(root.join("README.md"), b"# dirty\n").unwrap();
        fs::write(root.join("scratch.txt"), b"untracked scratch\n").unwrap();
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("dir/extra.txt"), b"nested untracked\n").unwrap();
        let sts = repo.statuses(Some(&mut so)).unwrap();
        let mut dirty = BTreeMap::new();
        for e in sts.iter() {
            dirty.insert(e.path().unwrap().to_string(), format!("{:?}", e.status()));
        }
        println!("status dirty entries = {}", dirty.len());
        for (p, s) in &dirty {
            println!("status {p} {s}");
        }

        fs::write(root.join("README.md"), README2).unwrap();
        fs::remove_file(root.join("scratch.txt")).unwrap();
        fs::remove_dir_all(root.join("dir")).unwrap();
        let sts = repo.statuses(Some(&mut so)).unwrap();
        println!("status restored entries = {}", sts.len());
        assert_eq!(sts.len(), 0);

        // ---- ⑧ index 条目（BTree 序）----
        let index = repo.index().unwrap();
        println!("index entries = {}", index.len());
        for e in index.iter() {
            println!(
                "idx {:06o} {} {}",
                e.mode,
                String::from_utf8_lossy(&e.path),
                e.id
            );
        }

        // ---- ⑨ workdir 文件清单（递归、BTreeSet 序）----
        let mut files = BTreeSet::new();
        list_workdir(&root, "", &mut files);
        println!("workdir files = {files:?}");

        // ---- ⑩ 错误路径：缺引用 / 零 oid / 非仓库 ----
        match repo.find_reference("refs/heads/nope") {
            Ok(_) => println!("nope ref unexpected ok"),
            Err(e) => println!("nope ref err = {:?}/{:?}", e.code(), e.class()),
        }
        match repo.find_tag(Oid::zero()) {
            Ok(_) => println!("zero tag unexpected ok"),
            Err(e) => println!("zero tag err = {:?}/{:?}", e.code(), e.class()),
        }
        let notrepo = std::env::temp_dir().join("mirvm_corpus_libgit2_notrepo");
        if notrepo.exists() {
            fs::remove_dir_all(&notrepo).unwrap();
        }
        fs::create_dir_all(&notrepo).unwrap();
        match Repository::open(&notrepo) {
            Ok(_) => println!("open non-repo unexpected ok"),
            Err(e) => println!("open non-repo err = {:?}/{:?}", e.code(), e.class()),
        }
        fs::remove_dir_all(&notrepo).unwrap();
    }

    // ---- ⑪ 清理 ----
    fs::remove_dir_all(&root).unwrap();
    println!("cleanup exists = {}", root.exists());
}
