#!/usr/bin/env mirvm
---
[dependencies]
git2 = { version = "=0.20.4", default-features = false }
libgit2-sys = "=0.18.5"
---
// git2 0.20.4 (libgit2 through the C FFI) + libgit2-sys pinned to =0.18.5. The pin
// matters because libgit2-sys wraps the C sources: an unpinned ^0.18.3 could drift
// onto a different libgit2 C version, so both crates are exact-pinned and the C
// surface is fixed at bundled libgit2 1.9.4. default-features = false drops the ssh
// and https features: this fixture is entirely local (init, file writes, commit,
// tree, log, status, refs) and needs no network, and the default set would pull in
// the libssh2-sys / openssl-sys / openssl-probe C builds. The host has no system
// libgit2 (pkg-config finds no libgit2.pc), so libgit2-sys builds the vendored
// libgit2 1.9.4 into a static archive with cc and mirvm loads it through the
// static-archive-to-shared-object closure channel; the heavy C build is expected.
//
// Test surface (the whole fixture uses the git2 API; no `git` subprocess is spawned):
//   libgit2 runtime version anchor (1.9.4) -> init_opts in a temp dir with
//   initial_head="master" (branch name pinned, the global gitconfig is not consulted)
//   -> std::fs writes three fixed files (README.md, src/main.rs, 32-byte seeded
//   xorshift data.bin) -> index.add_path in sorted order plus write and write_tree ->
//   two commits under fixed signatures (name/email/timestamp hard-coded, Time::new
//   with a constant +01:00 offset and no env), the second editing README and adding
//   src/lib.rs with the first commit as parent -> annotated v1.0 tag with a fixed
//   tagger -> read back and print: both commit ids and tree ids (pure content
//   addressing, sha1-anchored), HEAD name/shorthand/target, the revwalk log in
//   TIME|TOPOLOGICAL order (id/time/offset/parents/summary), the recursive tree
//   listing ({:06o} mode/kind/path/oid, canonical git order), get_path hit and miss,
//   the 5 blobs' len+fnv1a and byte-for-byte comparison with the source, the refs
//   list (BTreeMap order), the local branch list, HEAD/master reflog counts and
//   entries (old->new/msg), the status check (clean -> dirty: edited README plus two
//   untracked files, one nested, printing sorted path/flags -> restored clean), the
//   index entries (mode/path/oid, BTree order), and the three error paths.
//
// Determinism: all identities, timestamps and contents are hard-coded; commit/tree/tag
// use sha1 content addressing; every collection is a BTreeMap/BTreeSet; no HashMap
// order, RNG (xorshift is seeded), time, thread, absolute path or env value reaches
// the output. A native run measured 58 deterministic stdout lines, empty stderr, exit 0.
//
// Oracle: this fixture is one half of a native/differential pair. Its stdout is
// compared line for line against the same program built by real rustc and run
// natively, so every printed id, oid, ordering, count and flag must match and
// stderr must be empty on the success paths.
//
// Ids come from git objects read back from the repository rather than recomputed:
// commit, tree, blob and tag ids are sha1 over the object bytes, so identical bytes
// give identical ids under both engines. The two commit ids, both tree ids and the
// tag id are anchored this way, and assert_ne! on commits and trees confirms that
// the second commit really changed them.
//
// Error paths print the ErrorCode/ErrorClass enum Debug and never the Error message,
// whose text can embed an absolute path and would differ per machine.
//
// The fixture registers no Rust-to-C callbacks: tree traversal uses tree.iter()
// instead of the TreeWalk callback API, so the run stays off the callback ABI.
//
// Several results are asserted, not just printed: the two commit ids and tree ids
// differ, every blob equals its source bytes, and both clean status checks are zero.
//
// The status section is the only part that mutates the worktree: it asserts a clean
// tree, builds a dirty state (edited README plus two untracked files, one nested),
// prints the sorted path/flag pairs, restores the original bytes and asserts clean
// again. The recursive workdir listing skips .git and is BTreeSet-ordered.
//
// Printed paths are relative (src/main.rs, data.bin, dir/extra.txt) under a fixed
// temp-directory name, so no machine-specific absolute path reaches stdout.
//
// The reflog section reads both HEAD and refs/heads/master and prints every entry
// (old id -> new id plus message) in order, anchoring the reflog rewrite order.
//
// The data.bin blob is byte-identical in both trees, so the fixture also checks
// that identical content deduplicates to one blob object.
//
// Three-way re-run (repo root):
//   A: target/release/mirvm run tests/data/programs/c_libgit2.rs
//   B: cd "$(grep -l 'name = "c_libgit2"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_libgit2.rs
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use git2::{
    ObjectType, Oid, Repository, RepositoryInitOptions, Signature, Sort, StatusOptions, Time,
};

const NAME: &str = "Mirvm Tester";
const EMAIL: &str = "tester@example.com";
/// The two commits' fixed timestamps (seconds; the offset is always +01:00 = 60 minutes).
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

/// Seeded xorshift64* producing a 32-byte binary blob (binary-blob boundary).
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

/// Recursively collect tree traversal rows (order = tree entry order, canonical git order).
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

/// Recursively list workdir files (skipping .git); BTreeSet order keeps it deterministic.
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

    // Fixed subdirectory: cleared and recreated at the start, removed at the end (no accumulation).
    let root = std::env::temp_dir().join("mirvm_corpus_libgit2");
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();

    // git2 objects (Tree/Commit/Statuses/...) borrow the repo and implement Drop, so this
    // inner scope releases them in reverse order (repo is declared first and drops last);
    // the directory cleanup happens after the scope ends.
    {
        // ---- ① init + write files + two commits with fixed timestamps ----
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
        // Add in sorted order so the call order cannot influence the result.
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

        // Second commit: edit README.md, add src/lib.rs (workdir updated to match).
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

        // Annotated tag with a fixed tagger, to populate the refs list.
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

        // ---- ③ tree traversal + path lookup ----
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

        // ---- ④ blob read-back check (len/fnv/byte-for-byte comparison with the source) ----
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

        // ---- ⑤ refs list (BTreeMap order) + local branches ----
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

        // ---- ⑦ status: clean -> dirty -> restored clean ----
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

        // ---- ⑧ index entries (BTree order) ----
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

        // ---- ⑨ workdir file list (recursive, BTreeSet order) ----
        let mut files = BTreeSet::new();
        list_workdir(&root, "", &mut files);
        println!("workdir files = {files:?}");

        // ---- ⑩ error paths: missing ref / zero oid / non-repo ----
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

    // ---- ⑪ cleanup ----
    fs::remove_dir_all(&root).unwrap();
    println!("cleanup exists = {}", root.exists());
}
