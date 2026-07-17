#!/usr/bin/env mirvm
---
[dependencies]
# gix 0.69.1（0.6x 最新），minimal features：index（index_or_load_from_head 需要）
# + blob-diff（object::tree::diff 模块受它门控）。zlib 后端保持 gix 默认
# （gix-features/zlib → flate2 rust_backend = miniz_oxide）。
gix = { version = "0.69", default-features = false, features = ["index", "blob-diff"] }
---
// gix 0.69（纯 Rust git）端到端：init 仓库 → 固定内容写文件 → 固定 author/
// committer/时间戳 commit 两次（第二次改一文件加一文件）→ HEAD/log/tree 遍历/
// 两 commit tree diff 变更统计/blob 校验/refs+reflog/index 状态/错误路径。
// 全程 gix API，不 spawn git 命令。commit id 由「内容+固定签名+固定时间」完全
// 决定（sha1 内容寻址），打印全量 hex。native 侧输出全确定。
//
// 已知 FRONTIER（未能绕行，原生/镜像对拍停在首个 loose object 写入）：
// git loose object 是 zlib 容器，gix-odb 经 gix-features/zlib = flate2
// rust_backend（miniz_oxide + simd-adler32）做 inflate/deflate；zlib 容器的
// adler32 校验在单次 update ≥32B 时按运行期 CPUID 派发到 SIMD 实现的
// _mm(256)_sad_epu8（llvm.x86.avx2/sse2.psad.bw，mirvm 均未内建；guest CPUID
// 见 host feature 必命中，而 commit/tree 对象必然 ≥32B）。实测 trap 诊断：
// `llvm.x86.sse2.psad.bw`（__mm_sad_epu8，avx2 实现的 128 位归约段）。备选
// 后端同病或更糟：
//   * flate2 1.1 的 zlib-rs：flate2 强制开其 std feature，adler32 同为运行期
//     avx2 探测 + psad.bw；
//   * C libz（libz-sys static/stock）：flate2 C 后端无条件把 Rust 的
//     allocator::zalloc/zfree 两个 extern "C" fn 指针嵌进 z_stream 结构体传给
//     libz，libz 在 deflateInit2_/inflateInit2_ 里回调——mirvm 的 thunk 机制
//     只覆盖「显式 fn-ptr 实参」，结构体内嵌回调是盲区，宿主直接跳进 guest
//     数据地址 → SIGSEGV（si_addr==rip，无诊断；已用 LD_PRELOAD 实锤：
//     返回地址在 libz.so deflate 内）。C 路因此不可用。
//   （2026-07-17 注：P1 条目可执行化（decision-history §7.6）后 C 路已通——
//     extern "C" 回调的 fn-ptr 值本身即 stub 码址，内嵌逃逸直落可执行入口，
//     负对照探针实测 zlib 往返完成。本 driver 仍走 rust_backend，仅沿用 gix
//     默认后端，非再受盲区所限。）
// sha1 走 gix-odb 默认的 sha1_smol（纯 Rust，无 SIMD）。crc32fast 只服务
// pack/gzip 路径，本 driver 全程 loose object，不触达。
use std::convert::Infallible;
use std::fs;
use std::path::Path;

use gix::bstr::BStr;
use gix::date::time::Sign;
use gix::date::Time;
use gix::objs::tree::{EntryKind, EntryMode};
use gix::refs::transaction::PreviousValue;
use gix::Repository;

/// 两个 commit 的固定时间戳（秒，UTC+01:00）。
const T1: i64 = 1_700_000_000;
const T2: i64 = 1_700_000_600;

const README1: &[u8] = b"# mirvm gix pure\npure-Rust git differential sample.\n";
const README2: &[u8] =
    b"# mirvm gix pure\npure-Rust git differential sample.\nsecond commit updates this file.\n";
const MAIN_RS: &[u8] = b"fn main() {\n    println!(\"gix_pure\");\n}\n";
const LIB_RS: &[u8] = b"pub fn answer() -> u32 { 42 }\n";

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 固定身份的签名（author=committer 同一人，时间由调用方给）。
fn sig(secs: i64) -> gix::actor::SignatureRef<'static> {
    gix::actor::SignatureRef {
        name: BStr::new("Mirvm Tester"),
        email: BStr::new("tester@example.com"),
        time: Time {
            seconds: secs,
            offset: 3600,
            sign: Sign::Plus,
        },
    }
}

fn blob(repo: &Repository, data: &[u8]) -> gix::ObjectId {
    repo.write_blob(data).unwrap().detach()
}

/// 手工构 tree（git 规范序：按名字节序，目录条目视同带尾随 '/'）并写库。
fn tree_of(repo: &Repository, entries: &[(EntryKind, &str, gix::ObjectId)]) -> gix::ObjectId {
    let mut entries: Vec<gix::objs::tree::Entry> = entries
        .iter()
        .map(|(kind, name, oid)| gix::objs::tree::Entry {
            mode: EntryMode::from(*kind),
            filename: (*name).into(),
            oid: *oid,
        })
        .collect();
    entries.sort_by(|a, b| {
        let key = |e: &gix::objs::tree::Entry| {
            let mut k = e.filename.clone();
            if e.mode.kind() == EntryKind::Tree {
                k.push(b'/');
            }
            k
        };
        key(a).cmp(&key(b))
    });
    repo.write_object(&gix::objs::Tree { entries })
        .unwrap()
        .detach()
}

/// 递归收集 tree 遍历行（顺序 = tree 条目序，规范序 → 确定）。
fn walk_tree(repo: &Repository, tree: gix::ObjectId, prefix: &str, out: &mut Vec<String>) {
    let t = repo.find_tree(tree).unwrap();
    for e in t.decode().unwrap().entries.iter() {
        let path = format!("{prefix}{}", e.filename);
        out.push(format!(
            "{:06o} {:?} {} {}",
            e.mode.0,
            e.mode.kind(),
            path,
            e.oid
        ));
        if e.mode.kind() == EntryKind::Tree {
            walk_tree(repo, e.oid.into(), &format!("{path}/"), out);
        }
    }
}

/// 递归列出 workdir 文件（跳过 .git），排序保证确定。
fn list_workdir(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name == ".git" {
            continue;
        }
        let rel = format!("{prefix}{name}");
        let p = dir.join(&name);
        if p.is_dir() {
            list_workdir(&p, &format!("{rel}/"), out);
        } else {
            out.push(rel);
        }
    }
}

/// tree diff 并收集人类可读变更行；返回 (行, 增/删/改计数)。
fn diff_trees(
    old: &gix::Tree<'_>,
    new: &gix::Tree<'_>,
) -> (Vec<String>, (usize, usize, usize)) {
    use gix::object::tree::diff::{Action, Change};
    let mut lines = Vec::new();
    let (mut adds, mut dels, mut mods) = (0, 0, 0);
    old.changes()
        .unwrap()
        .options(|o| {
            o.track_path();
            o.track_rewrites(None);
        })
        .for_each_to_obtain_tree(new, |change| {
            match change {
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    adds += 1;
                    lines.push(format!("A {} {:06o} {}", location, entry_mode.0, id));
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    dels += 1;
                    lines.push(format!("D {} {:06o} {}", location, entry_mode.0, id));
                }
                Change::Modification {
                    location,
                    previous_entry_mode,
                    previous_id,
                    entry_mode,
                    id,
                } => {
                    mods += 1;
                    lines.push(format!(
                        "M {} {:06o}->{:06o} {}->{}",
                        location, previous_entry_mode.0, entry_mode.0, previous_id, id
                    ));
                }
                Change::Rewrite { .. } => lines.push("R ?".to_string()),
            }
            Ok::<Action, Infallible>(Action::Continue)
        })
        .unwrap();
    (lines, (adds, dels, mods))
}

fn main() {
    // 固定子目录：开头清一次再建，结尾删除——多跑不累加。
    let root = std::env::temp_dir().join("mirvm_corpus_gix_pure");
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();

    // ---- ① init + 写文件 + 两个固定时间戳的 commit ----
    let repo = gix::init(&root).unwrap();
    println!("repo kind = {:?}", repo.kind());
    println!("head unborn = {}", repo.head().unwrap().is_unborn());

    // data.bin：定种 xorshift 32 字节（二进制 blob 边界）。
    let mut data = Vec::new();
    let mut x = 0x9E3779B97F4A7C15u64;
    while data.len() < 32 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        data.extend_from_slice(&x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
    }
    data.truncate(32);

    fs::create_dir_all(root.join("src")).unwrap();
    for (p, c) in [
        ("README.md", README1),
        ("src/main.rs", MAIN_RS),
        ("data.bin", data.as_slice()),
    ] {
        fs::write(root.join(p), c).unwrap();
    }

    let b_readme1 = blob(&repo, README1);
    let b_main = blob(&repo, MAIN_RS);
    let b_data = blob(&repo, &data);
    let b_readme1_again = blob(&repo, README1);
    println!("blob dedup same-id = {}", b_readme1_again == b_readme1);

    let t_src1 = tree_of(&repo, &[(EntryKind::Blob, "main.rs", b_main)]);
    let t_root1 = tree_of(
        &repo,
        &[
            (EntryKind::Blob, "README.md", b_readme1),
            (EntryKind::Blob, "data.bin", b_data),
            (EntryKind::Tree, "src", t_src1),
        ],
    );
    let c1 = repo
        .commit_as(
            sig(T1),
            sig(T1),
            "HEAD",
            "initial commit: add base files\n\nREADME.md, src/main.rs, data.bin\n",
            t_root1,
            std::iter::empty::<gix::ObjectId>(),
        )
        .unwrap()
        .detach();
    println!("commit1 = {c1}");
    println!("tree1 = {t_root1}");

    // 第二个 commit：改 README.md，加 src/lib.rs（workdir 同步更新）。
    fs::write(root.join("README.md"), README2).unwrap();
    fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();
    let b_readme2 = blob(&repo, README2);
    let b_lib = blob(&repo, LIB_RS);
    let t_src2 = tree_of(
        &repo,
        &[
            (EntryKind::Blob, "lib.rs", b_lib),
            (EntryKind::Blob, "main.rs", b_main),
        ],
    );
    let t_root2 = tree_of(
        &repo,
        &[
            (EntryKind::Blob, "README.md", b_readme2),
            (EntryKind::Blob, "data.bin", b_data),
            (EntryKind::Tree, "src", t_src2),
        ],
    );
    let c2 = repo
        .commit_as(
            sig(T2),
            sig(T2),
            "HEAD",
            "second commit: update README, add lib.rs\n",
            t_root2,
            [c1],
        )
        .unwrap()
        .detach();
    println!("commit2 = {c2}");
    println!("tree2 = {t_root2}");

    // 附一个 tag（annotated，固定 tagger 时间）充实 refs 列表。
    let tag_ref = repo
        .tag(
            "v1.0",
            &c1,
            gix::objs::Kind::Commit,
            Some(sig(T1)),
            "release v1.0\n",
            PreviousValue::MustNotExist,
        )
        .unwrap();
    println!("tag ref = {}", tag_ref.name().as_bstr());

    // ---- ② HEAD / log ----
    println!("head name = {}", repo.head_name().unwrap().unwrap());
    println!("head id = {}", repo.head_id().unwrap().detach());
    let head = repo.head_commit().unwrap();
    let msg = head.message().unwrap();
    println!("head title = {}", msg.title);
    println!("head body present = {}", msg.body.is_some());
    let author = head.author().unwrap();
    let committer = head.committer().unwrap();
    for (label, s) in [("author", author), ("committer", committer)] {
        println!(
            "head {label} = {} <{}> {} {}",
            s.name, s.email, s.time.seconds, s.time.offset
        );
    }
    println!(
        "head parents = {:?}",
        head.parent_ids()
            .map(|i| i.detach().to_string())
            .collect::<Vec<_>>()
    );
    println!("commit1 time = {}", repo.find_commit(c1).unwrap().time().unwrap().seconds);

    let mut nlog = 0;
    for info in head.ancestors().all().unwrap() {
        let info = info.unwrap();
        println!("log {} time={:?}", info.id, info.commit_time);
        nlog += 1;
    }
    println!("log count = {nlog}");

    // ---- ③ tree 遍历 + 路径查找 ----
    let mut rows = Vec::new();
    walk_tree(&repo, t_root2, "", &mut rows);
    println!("tree2 entries = {}", rows.len());
    for r in &rows {
        println!("tree {r}");
    }
    let t2 = repo.find_tree(t_root2).unwrap();
    let e = t2.lookup_entry_by_path("src/main.rs").unwrap().unwrap();
    println!("lookup src/main.rs = {:06o} {}", e.mode().0, e.oid());
    println!(
        "lookup nope.txt = {}",
        t2.lookup_entry_by_path("nope.txt").unwrap().is_some()
    );

    // ---- ④ blob 读取校验（kind/长度/fnv/逐字节比对原文）----
    for (label, oid, expect) in [
        ("readme1", b_readme1, README1),
        ("readme2", b_readme2, README2),
        ("main", b_main, MAIN_RS),
        ("lib", b_lib, LIB_RS),
        ("data", b_data, data.as_slice()),
    ] {
        let obj = repo.find_object(oid).unwrap();
        println!(
            "blob {label} kind={:?} len={} fnv={:016x} ok={}",
            obj.kind,
            obj.data.len(),
            fnv1a(&obj.data),
            obj.data == expect
        );
    }

    // ---- ⑤ 两 commit 的 tree diff（变更统计，双向）----
    let ct1 = repo.find_commit(c1).unwrap().tree().unwrap();
    let ct2 = repo.find_commit(c2).unwrap().tree().unwrap();
    let (fwd, (a, d, m)) = diff_trees(&ct1, &ct2);
    println!("changes c1->c2 n={} A={a} D={d} M={m}", fwd.len());
    for l in &fwd {
        println!("diff {l}");
    }
    let (rev, (a, d, m)) = diff_trees(&ct2, &ct1);
    println!("changes c2->c1 n={} A={a} D={d} M={m}", rev.len());
    for l in &rev {
        println!("rdiff {l}");
    }

    // ---- ⑥ refs 列表 + HEAD reflog ----
    let mut nrefs = 0;
    for r in repo.references().unwrap().all().unwrap() {
        let r = r.unwrap();
        let target = match r.target() {
            gix::refs::TargetRef::Object(oid) => oid.to_string(),
            gix::refs::TargetRef::Symbolic(name) => format!("-> {}", name.as_bstr()),
        };
        println!("ref {} = {target}", r.name().as_bstr());
        nrefs += 1;
    }
    println!("refs count = {nrefs}");
    let branches: Vec<String> = repo
        .references()
        .unwrap()
        .local_branches()
        .unwrap()
        .map(|r| r.unwrap().name().as_bstr().to_string())
        .collect();
    println!("local branches = {branches:?}");

    let head_ref = repo.find_reference("HEAD").unwrap();
    let mut log_iter = head_ref.log_iter();
    match log_iter.all().unwrap() {
        Some(iter) => {
            let mut n = 0;
            for line in iter {
                let line = line.unwrap();
                println!(
                    "reflog {} -> {} msg={}",
                    line.previous_oid(),
                    line.new_oid(),
                    line.message
                );
                n += 1;
            }
            println!("reflog count = {n}");
        }
        None => println!("reflog none"),
    }

    // ---- ⑦ index 状态（从 HEAD tree 重建的内存 index）----
    let index = repo.index_or_load_from_head().unwrap();
    println!("index entries = {}", index.entries().len());
    for e in index.entries() {
        println!(
            "idx {:06o} {} {}",
            e.mode.bits(),
            e.path_in(index.path_backing()),
            e.id
        );
    }

    // ---- ⑧ workdir 文件清单（递归、排序）----
    let mut files = Vec::new();
    list_workdir(&root, "", &mut files);
    println!("workdir files = {files:?}");

    // ---- ⑨ 错误路径：缺对象 / 缺引用 / 非仓库 / 父 commit 不匹配 ----
    let null = gix::hash::ObjectId::null(gix::hash::Kind::Sha1);
    match repo.find_object(null) {
        Ok(_) => println!("null obj unexpected ok"),
        Err(e) => println!("null obj err = {e}"),
    }
    match repo.find_reference("refs/heads/nope") {
        Ok(_) => println!("nope ref unexpected ok"),
        Err(e) => println!("nope ref err = {e}"),
    }
    match gix::open(root.join("not-a-repo")) {
        Ok(_) => println!("open non-repo unexpected ok"),
        Err(e) => println!("open non-repo err = {e}"),
    }
    // 期望 PreviousValue 不匹配：写出一个 dangling commit 后 ref 更新失败，HEAD 不变。
    match repo.commit_as(
        sig(T2),
        sig(T2),
        "HEAD",
        "bad commit\n",
        t_root2,
        [null],
    ) {
        Ok(_) => println!("bad-parent commit unexpected ok"),
        Err(e) => println!("bad-parent commit err = {e}"),
    }
    println!(
        "head after failed commit = {}",
        repo.head_id().unwrap().detach()
    );

    // ---- ⑩ 清理 ----
    fs::remove_dir_all(&root).unwrap();
    println!("cleanup exists = {}", root.exists());
}
