#!/usr/bin/env mirvm
---
[dependencies]
# gix 0.69.1 (latest 0.6x), minimal features: index (needed by index_or_load_from_head)
# + blob-diff (it gates the object::tree::diff module). The zlib backend stays at gix's
# default (gix-features/zlib -> flate2 rust_backend = miniz_oxide).
gix = { version = "0.69", default-features = false, features = ["index", "blob-diff"] }
---
// gix 0.69 (pure-Rust git) end to end: init a repository, write files with fixed content, and
// make two commits with fixed author/committer/timestamps (the second modifies one file and adds
// another), then exercise HEAD/log/tree walk, two-commit tree diff statistics, blob verification,
// refs+reflog/index state, and error paths. All through the gix API, never spawning the git
// command. The commit id is fully determined by content + fixed signature + fixed time (sha1
// content addressing) and prints as full hex; the native side is fully deterministic.
//
// Known FRONTIER (not bypassed; the native/mirror differential stops at the first loose object
// write): a git loose object is a zlib container, and gix-odb inflates/deflates through
// gix-features/zlib = flate2 rust_backend (miniz_oxide + simd-adler32). When a single update
// is >=32B, the zlib adler32 check dispatches at runtime on CPUID to a SIMD implementation
// using _mm(256)_sad_epu8 (llvm.x86.avx2/sse2.psad.bw, neither built into mirvm; the guest
// CPUID sees the host features and always hits, while commit/tree objects are >=32B). The
// observed trap diagnostic is 'llvm.x86.sse2.psad.bw' (__mm_sad_epu8, the 128-bit reduction
// leg of the avx2 implementation). The alternative backends are equally bad or worse:
//   * flate2 1.1's zlib-rs: flate2 forces its std feature on, and its adler32 again does
//     runtime avx2 detection plus psad.bw;
//   * C libz (libz-sys static/stock): the flate2 C backend unconditionally embeds the two
//     Rust extern "C" fn pointers allocator::zalloc/zfree into the z_stream struct it hands
//     to libz, which calls them back from deflateInit2_/inflateInit2_. mirvm's thunk
//     mechanism only covers explicit fn-ptr arguments, so a callback embedded in a struct is
//     a blind spot and the host jumps straight to a guest data address -> SIGSEGV
//     (si_addr == rip, no diagnostic; confirmed with LD_PRELOAD, the return address lies
//     inside libz.so deflate). That extern "C" callback embedded in a struct is the hazard this
//     driver avoids by using rust_backend, gix's default backend; the C path is not exercised here.
// sha1 uses gix-odb's default sha1_smol (pure Rust, no SIMD). crc32fast only serves the
// pack/gzip paths; this driver writes and reads loose objects throughout and never reaches
// them, so neither participates in the differential.
use std::convert::Infallible;
use std::fs;
use std::path::Path;

use gix::bstr::BStr;
use gix::date::time::Sign;
use gix::date::Time;
use gix::objs::tree::{EntryKind, EntryMode};
use gix::refs::transaction::PreviousValue;
use gix::Repository;

/// Fixed timestamps for the two commits (seconds, UTC+01:00).
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

/// Fixed-identity signature (same author and committer; the caller supplies the time).
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

/// Build a tree in git canonical order (name byte order; tree entries sort with a trailing '/').
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

/// Collect tree-walk rows recursively (order = tree entry order, canonical and deterministic).
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

/// List workdir files recursively, skipping .git; sorting keeps it deterministic.
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

/// Diff two trees into human-readable change lines; returns (lines, (added, deleted, modified)).
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
    // Fixed subdirectory: wiped/recreated up front and removed at the end, so reruns do not accumulate.
    let root = std::env::temp_dir().join("mirvm_corpus_gix_pure");
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();

    // ---- ① init + write files + two commits at fixed timestamps ----
    let repo = gix::init(&root).unwrap();
    println!("repo kind = {:?}", repo.kind());
    println!("head unborn = {}", repo.head().unwrap().is_unborn());

    // data.bin: 32 seeded xorshift bytes (a binary blob boundary case).
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

    // Second commit: modify README.md, add src/lib.rs (the workdir is updated in step).
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

    // Add one annotated tag with a fixed tagger time to populate the refs list.
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

    // ---- ③ tree walk + path lookup ----
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

    // ---- ④ blob read check (kind/length/fnv/byte-for-byte against the source) ----
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

    // ---- ⑤ tree diff of the two commits (change stats, both directions) ----
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

    // ---- ⑥ refs list + HEAD reflog ----
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

    // ---- ⑦ index state (in-memory index rebuilt from the HEAD tree) ----
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

    // ---- ⑧ workdir file listing (recursive, sorted) ----
    let mut files = Vec::new();
    list_workdir(&root, "", &mut files);
    println!("workdir files = {files:?}");

    // ---- ⑨ error paths: missing object / missing ref / non-repo / parent-commit mismatch ----
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
    // Expected PreviousValue mismatch: the dangling commit is written, the ref update fails, HEAD stays.
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

    // ---- ⑩ cleanup ----
    fs::remove_dir_all(&root).unwrap();
    println!("cleanup exists = {}", root.exists());
}
