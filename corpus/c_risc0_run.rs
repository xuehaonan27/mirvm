#!/usr/bin/env mirvm
---
[dependencies]
risc0-zkvm = { version = "=3.0.6", default-features = false, features = ["prove"] }
risc0-binfmt = "=3.0.5"
risc0-zkos-v1compat = "=2.2.3"
---
// risc0-zkvm execute-only differential, status expected-red. A hand-written
// rv32im guest runs through default_executor inside an ExecutorEnv, at no point
// proving anything; the journal bytes are hashed with FNV and the public values
// are asserted. The driver is complete: the native oracle block below is the
// fixed 13-line output, while mirvm default and JIT hit the same engine boundary
// described further down.
// Form and version pin (all three points were verified on the machine):
//   * features=["prove"], not the ["std"] one would expect for execute-only.
//     Since 3.0.6 the in-process executor is feature gated in two levels:
//     without prove, default_executor() returns ExternalProver("ipc", r0vm path),
//     which spawns an r0vm subprocess that this machine does not have, and that
//     IPC channel is outside what this driver tests. With prove enabled,
//     default_executor goes through LocalProver::execute and ExecutorImpl::from_elf
//     in process. The prover itself is never called (no Receipt, no proof, no
//     Fiat-Shamir randomness), so execute-only still holds. The dependency closure
//     is 247 normal crates (cargo tree); three -sys crates compile C++ kernels
//     with cc/g++, needing no cmake/bindgen/clang/nasm.
//   * risc0-zkvm-methods (the official prebuilt guest ELF crate) is not published
//     on crates.io (the API returns 404) and the risc0-build path would need the
//     cargo-risczero toolchain, which is absent. So the driver hand-writes the
//     guest: a built-in rv32im mini assembler (fixed-width li, two-pass
//     addressing) assembles the machine code and wraps it in a minimal ELF32
//     (EM_RISCV/ET_EXEC, a single PT_LOAD), with no external toolchain.
//   * Since 3.0.6 execute() takes a ProgramBinary container rather than a bare
//     ELF (user ELF + kernel ELF; ExecutorImpl::from_elf -> ProgramBinary::decode
//     validates AbiKind::V1Compat/^1.0.0). The kernel is the prebuilt V1COMPAT_ELF
//     embedded in the risc0-zkos-v1compat 2.2.3 crate (the syscall v1
//     compatibility layer). risc0-binfmt and risc0-zkos-v1compat are pinned to the
//     exact versions risc0-zkvm 3.0.6 requires (=3.0.5 and =2.2.3), and
//     ProgramBinary::new().encode() is public API.
// What the driver exercises:
//   (1) Both ExecutorEnv write channels: write(&u32) twice (risc0 serde to_vec,
//       one little-endian word each) plus write_slice of the 32-byte digest, 40
//       bytes of stdin in total.
//   (2) The guest's three sys_read(fd=STDIN) calls: 8 bytes of input, the 32-byte
//       digest, then an EOF probe whose 4-byte request must return 0. Each nread
//       return value is checked, and a mismatch jumps to fail.
//   (3) Guest arithmetic and assertion: p = 37*41 through the M-extension mul,
//       with a bne asserting p == 1517, then q = p*7+13. The failure path writes
//       the 0xDEADBEEF marker to the journal and exits 1, which the differential
//       comparison would catch.
//   (4) env::commit public output: sys_write(fd=JOURNAL, [a,b,p,q]) as 16 bytes.
//   (5) sys_halt(user_exit=0, output_digest): a zero halt digest discards the
//       journal (SessionInfo.journal=None), so the digest must be non-zero and the
//       driver gets it bit-exact. The host precomputes it with risc0's own types,
//       Output{journal: Pruned(SHA-256(journal)), assumptions: Pruned(ZERO)}
//       .digest(), which is the same finalize formula guest env::exit uses, and
//       passes it in over stdin for the guest to forward. Printing
//       receipt_claim.output.digest() then proves the digest really travelled
//       through the guest halt register.
//   (6) Session shape: segments=1, po2, user cycles and the ProgramBinary byte
//       count are all printed as deterministic functions of the executor.
// Determinism: fixed inputs, guest machine code from a fixed assembler, no time,
// randomness, thread ordering (LocalProver::execute is single-threaded; rayon
// only appears on the proving side) or environment dependence; no tracing
// subscriber is registered, so risc0's tracing events are dropped and stderr
// stays empty; error paths panic instead of printing. The output is 13 lines.
// ABI facts the driver is written against: the SOFTWARE ecall register
// convention is t0=2/t6=nr/a0=buf/a1=len/a2=name/a3..=args; syscall numbers are
// Read=12 and Write=16; fileno STDIN=0 and JOURNAL=3; sys_halt takes
// a0=TERMINATE|(user_exit<<8) and a1 as an OutDigest pointer; a SyscallName is a
// raw pointer to a NUL-terminated name string in the guest data segment;
// TEXT_START=0x0020_0800 and KERNEL_START=0xC000_0000 bound the user ELF, whose
// entry is jumped to through the 0x0001_0000 slot; and the ProgramBinary on-disk
// format is MAGIC + version + postcard header + user/kernel length prefixes,
// which encode() handles.
// Engine boundary (not a driver problem): through the prove/default features the
// three REQUIRED risc0 circuit crates (rv32im, recursion, keccak) each pull a -sys
// crate whose build.rs unconditionally compiles kernels/cxx/*.cpp into a static
// archive with cc (the only switch is CUDA, there is no way to turn C++ off). All
// three archives export the same weak C++ sized-delete COMDAT symbol _ZdlPvS_.
// mirvm turns each whole archive into a shared object and exports everything, so
// the name becomes visible in two dynsym tables and reject_symbol_ambiguity
// refuses it by design. Native has no ambiguity: archive members are pulled
// lazily and the first weak definition wins, which the green native run
// demonstrates. The fix on the engine side is either resolving same-name weak
// symbols during materialization (first wins, recorded) or a member-level lazy
// pull model.
// There is no legitimate workaround: the -sys C++ build has no feature or env
// switch (the only build.rs knob is an extra CUDA toggle); risc0-zkvm's
// in-process executor requires the prove feature (without it default_executor
// spawns an r0vm subprocess that does not exist here) and prove drags in
// circuit-*/prove (cargo has no negative features); and pinning older versions
// does not escape, since 2.3.2 also REQUIRED the same three circuit crates.
// Smallest reproduction: a cargo-script that depends only on
// risc0-circuit-rv32im-sys =4.0.3 and risc0-circuit-recursion-sys =4.0.3 and
// prints one line already hits the same panic during materialization (its
// content-addressed .so hash is identical across runs).
// Wiring: red_code=101 (rustc's standard exit code for a panicking thread);
// red_pattern="静态归档导出符号 `_ZdlPvS_` 同时来自" is a stable substring, while
// the archive hash and the thread number in the panic header vary per run, so no
// full-string match is possible. Under expected-red the stderr of the three
// dimensions is naturally not byte-identical because of the thread number.
// Native oracle (cold build 2m23s, md5-stable across reruns, 13 lines):
//   user elf bytes = 911
//   program binary bytes = 33335
//   exit = Halted(0)
//   segments = 1
//   cycles total = 485
//   segment[0] po2 = 14 cycles = 485
//   journal len = 16
//   journal words = [37, 41, 1517, 10632]
//   journal fnv1a = fbb47fc52544af18
//   journal matches host precompute = true
//   claim output digest = 346f1151e5248896285018e25bda148148eacadb1e540c53774c7c3bb5571d9c
//   claim digest == host precompute = true
//   risc0 execute-only ok
// Once the engine boundary closes, the mirvm dimensions must reproduce these 13
// lines byte-for-byte. The dependency closure is 247 normal crates (324
// including build dependencies); the three -sys crates need cc/g++ but no
// cmake/bindgen/clang/nasm.
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
use risc0_zkvm::sha::{Digestible, Impl, Sha256};
use risc0_zkvm::{default_executor, Digest, ExecutorEnv, ExitCode, MaybePruned, Output};

// ---------------- rv32im mini assembler (fixed-width li, two-pass) ----------------
const TEXT_START: u32 = 0x0020_0800; // risc0-zkvm-platform memory::TEXT_START

// register numbers (risc0-zkvm-platform syscall::reg_abi)
const X0: u8 = 0;
const T0: u8 = 5;
const T1: u8 = 6;
const T2: u8 = 7;
const S0: u8 = 8;
const S1: u8 = 9;
const A0: u8 = 10;
const A1: u8 = 11;
const A2: u8 = 12;
const A3: u8 = 13;
const A4: u8 = 14;
const A5: u8 = 15;
const S2: u8 = 18;
const S3: u8 = 19;
const T6: u8 = 31;

const ECALL: u32 = 0x0000_0073;

fn enc_r(f7: u32, rs2: u8, rs1: u8, f3: u32, rd: u8) -> u32 {
    (f7 << 25) | ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | (f3 << 12) | ((rd as u32) << 7)
        | 0x33
}
fn enc_i(imm: i32, rs1: u8, f3: u32, rd: u8, op: u32) -> u32 {
    (((imm as u32) & 0xfff) << 20)
        | ((rs1 as u32) << 15)
        | (f3 << 12)
        | ((rd as u32) << 7)
        | op
}
fn enc_s(imm: i32, rs2: u8, rs1: u8, f3: u32) -> u32 {
    let im = (imm as u32) & 0xfff;
    ((im >> 5) << 25)
        | ((rs2 as u32) << 20)
        | ((rs1 as u32) << 15)
        | (f3 << 12)
        | ((im & 0x1f) << 7)
        | 0x23
}
fn enc_b(imm: i32, rs2: u8, rs1: u8, f3: u32) -> u32 {
    let im = imm as u32;
    (((im >> 12) & 1) << 31)
        | (((im >> 5) & 0x3f) << 25)
        | ((rs2 as u32) << 20)
        | ((rs1 as u32) << 15)
        | (f3 << 12)
        | (((im >> 1) & 0xf) << 8)
        | (((im >> 11) & 1) << 7)
        | 0x63
}

// finish needs a branch register, so branches record it immediately
struct Asm2 {
    words: Vec<u32>,
    labels: Vec<(&'static str, usize)>,
    fix: Vec<(usize, u8, u8, u32, &'static str)>, // idx, rs1, rs2, f3, label
}

impl Asm2 {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            labels: Vec::new(),
            fix: Vec::new(),
        }
    }
    fn emit(&mut self, w: u32) {
        self.words.push(w);
    }
    fn li(&mut self, rd: u8, val: u32) {
        let hi = (val.wrapping_add(0x800) >> 12) & 0xF_FFFF;
        let lo = (val as i64) - ((hi as i64) << 12);
        debug_assert!((-2048..=2047).contains(&lo));
        self.emit((hi << 12) | ((rd as u32) << 7) | 0x37);
        self.emit(enc_i(lo as i32, rd, 0, rd, 0x13));
    }
    fn li_data(&mut self, rd: u8, name: &str, resolve: &dyn Fn(&str) -> u32) {
        self.li(rd, resolve(name));
    }
    fn addi(&mut self, rd: u8, rs1: u8, imm: i32) {
        self.emit(enc_i(imm, rs1, 0, rd, 0x13));
    }
    fn lw(&mut self, rd: u8, off: i32, rs1: u8) {
        self.emit(enc_i(off, rs1, 2, rd, 0x03));
    }
    fn sw(&mut self, rs2: u8, off: i32, rs1: u8) {
        self.emit(enc_s(off, rs2, rs1, 2));
    }
    fn mul(&mut self, rd: u8, rs1: u8, rs2: u8) {
        self.emit(enc_r(1, rs2, rs1, 0, rd));
    }
    fn bne(&mut self, rs1: u8, rs2: u8, label: &'static str) {
        self.fix.push((self.words.len(), rs1, rs2, 1, label));
        self.emit(0);
    }
    fn label(&mut self, name: &'static str) {
        self.labels.push((name, self.words.len()));
    }
    fn finish(mut self) -> Vec<u32> {
        let fix = self.fix.clone();
        for (idx, rs1, rs2, f3, label) in fix {
            let target = self
                .labels
                .iter()
                .find(|(n, _)| *n == label)
                .unwrap_or_else(|| panic!("label {label} missing"))
                .1;
            let off = ((target as i64 - idx as i64) * 4) as i32;
            self.words[idx] = enc_b(off, rs2, rs1, f3);
        }
        self.words
    }
}

const NAME_READ: &str = "risc0_zkvm_platform::syscall::nr::SYS_READ\0";
const NAME_WRITE: &str = "risc0_zkvm_platform::syscall::nr::SYS_WRITE\0";

// ecall numbers (risc0-zkvm-platform syscall::ecall)
const EC_HALT: u32 = 0;
const EC_SOFTWARE: u32 = 2;
// syscall numbers (the Syscall enum)
const NR_READ: u32 = 12;
const NR_WRITE: u32 = 16;
// fd（fileno）
const FD_STDIN: u32 = 0;
const FD_JOURNAL: u32 = 3;

/// Assembles the guest body. resolve maps a data symbol to its absolute address.
fn assemble_body(resolve: &dyn Fn(&str) -> u32) -> Vec<u32> {
    let mut a = Asm2::new();
    // ---- sys_read(fd=0, INBUF, 8): the two u32 inputs ----
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_READ);
    a.li_data(A0, "INBUF", resolve);
    a.li(A1, 8);
    a.li_data(A2, "NREAD", resolve);
    a.li(A3, FD_STDIN);
    a.li(A4, 8);
    a.emit(ECALL);
    a.li(T1, 8);
    a.bne(A0, T1, "fail"); // nread != 8 → fail
    // a = INBUF[0], b = INBUF[1]
    a.li_data(T2, "INBUF", resolve);
    a.lw(S0, 0, T2);
    a.lw(S1, 4, T2);
    // ---- sys_read(fd=0, DIGBUF, 32): the host-computed Output digest, 8 words ----
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_READ);
    a.li_data(A0, "DIGBUF", resolve);
    a.li(A1, 32);
    a.li_data(A2, "NREAD", resolve);
    a.li(A3, FD_STDIN);
    a.li(A4, 32);
    a.emit(ECALL);
    a.li(T1, 32);
    a.bne(A0, T1, "fail");
    // ---- EOF probe: sys_read(fd=0, OUTBUF, 4) must return 0 ----
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_READ);
    a.li_data(A0, "OUTBUF", resolve);
    a.li(A1, 4);
    a.li_data(A2, "NREAD", resolve);
    a.li(A3, FD_STDIN);
    a.li(A4, 4);
    a.emit(ECALL);
    a.bne(A0, X0, "fail");
    // ---- p = a * b, asserting p == 1517 ----
    a.mul(S2, S0, S1);
    a.li(T1, 1517);
    a.bne(S2, T1, "fail");
    // ---- q = p * 7 + 13 ----
    a.li(T1, 7);
    a.mul(S3, S2, T1);
    a.addi(S3, S3, 13);
    // ---- OUTBUF = [a, b, p, q] ----
    a.li_data(T2, "OUTBUF", resolve);
    a.sw(S0, 0, T2);
    a.sw(S1, 4, T2);
    a.sw(S2, 8, T2);
    a.sw(S3, 12, T2);
    // ---- sys_write(fd=JOURNAL, OUTBUF, 16), i.e. env::commit public output ----
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_WRITE);
    a.li(A0, 0);
    a.li(A1, 0);
    a.li_data(A2, "NWRITE", resolve);
    a.li(A3, FD_JOURNAL);
    a.li_data(A4, "OUTBUF", resolve);
    a.li(A5, 16);
    a.emit(ECALL);
    // ---- sys_halt(user_exit=0, DIGBUF) ----
    a.li(T0, EC_HALT);
    a.li(A0, 0); // halt::TERMINATE | (0 << 8)
    a.li_data(A1, "DIGBUF", resolve);
    a.emit(ECALL);
    a.emit(0); // an illegal instruction as a floor (unreachable)
    // ---- fail: write the 0xDEADBEEF marker to the journal and exit 1 ----
    a.label("fail");
    a.li(T1, 0xDEAD_BEEF);
    a.li_data(T2, "OUTBUF", resolve);
    a.sw(T1, 0, T2);
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_WRITE);
    a.li(A0, 0);
    a.li(A1, 0);
    a.li_data(A2, "NWRITE", resolve);
    a.li(A3, FD_JOURNAL);
    a.li_data(A4, "OUTBUF", resolve);
    a.li(A5, 4);
    a.emit(ECALL);
    a.li(T0, EC_HALT);
    a.li(A0, 1 << 8); // halt::TERMINATE | (1 << 8)
    a.li_data(A1, "DIGBUF", resolve);
    a.emit(ECALL);
    a.emit(0);
    a.finish()
}

/// Two-pass addressing: resolve data symbols to 0 to learn the code length, then re-assemble.
fn build_user_elf() -> Vec<u8> {
    let zero = |_: &str| 0u32;
    let code_len = assemble_body(&zero).len() * 4;

    let data_base = TEXT_START + (code_len as u32 + 15) / 16 * 16;
    let inbuf = data_base;
    let outbuf = inbuf + 8;
    let digbuf = outbuf + 16;
    let nread = digbuf + 32;
    let nwrite = nread + NAME_READ.len() as u32;
    let data_end = nwrite + NAME_WRITE.len() as u32;

    let resolve = |name: &str| -> u32 {
        match name {
            "INBUF" => inbuf,
            "OUTBUF" => outbuf,
            "DIGBUF" => digbuf,
            "NREAD" => nread,
            "NWRITE" => nwrite,
            _ => panic!("unknown sym {name}"),
        }
    };
    let words = assemble_body(&resolve);

    // Segment = code + alignment padding + data (INBUF/OUTBUF/DIGBUF zeroed) + names
    let mut seg = Vec::new();
    for w in &words {
        seg.extend_from_slice(&w.to_le_bytes());
    }
    while seg.len() < (data_base - TEXT_START) as usize {
        seg.push(0);
    }
    seg.resize(seg.len() + 8 + 16 + 32, 0);
    seg.extend_from_slice(NAME_READ.as_bytes());
    seg.extend_from_slice(NAME_WRITE.as_bytes());
    assert_eq!(seg.len() as u32, data_end - TEXT_START);

    // Minimal ELF32 (EM_RISCV / ET_EXEC / a single PT_LOAD)
    let mut elf = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1, 1, 0]); // ident: ELF32 LE
    elf.extend_from_slice(&[0; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&243u16.to_le_bytes()); // e_machine = EM_RISCV
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&TEXT_START.to_le_bytes()); // e_entry
    elf.extend_from_slice(&52u32.to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags (rv32im, no RVC)
    elf.extend_from_slice(&52u16.to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&32u16.to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    assert_eq!(elf.len(), 52);
    // program header
    let seg_off = 0x100u32;
    elf.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    elf.extend_from_slice(&seg_off.to_le_bytes()); // p_offset
    elf.extend_from_slice(&TEXT_START.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&TEXT_START.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&(seg.len() as u32).to_le_bytes()); // p_filesz
    elf.extend_from_slice(&(seg.len() as u32).to_le_bytes()); // p_memsz
    elf.extend_from_slice(&5u32.to_le_bytes()); // p_flags = R+X
    elf.extend_from_slice(&0x1000u32.to_le_bytes()); // p_align
    assert_eq!(elf.len(), 84);
    elf.resize(seg_off as usize, 0);
    elf.extend_from_slice(&seg);
    elf
}

// ---------------- host side ----------------

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn main() {
    // Fixed inputs and expectations (the guest recomputes and asserts them itself)
    let (a, b): (u32, u32) = (37, 41);
    let p: u32 = a * b; // 1517
    let q: u32 = p * 7 + 13; // 10632

    // expected journal = four little-endian u32s
    let mut journal_want = Vec::new();
    for w in [a, b, p, q] {
        journal_want.extend_from_slice(&w.to_le_bytes());
    }

    // The host precomputes env::exit's Output digest with risc0's own types:
    // journal_digest = SHA-256(journal); an empty assumptions list is Digest::ZERO.
    let journal_digest: Digest = *Impl::hash_bytes(&journal_want);
    let output = Output {
        journal: MaybePruned::Pruned(journal_digest),
        assumptions: MaybePruned::Pruned(Digest::ZERO),
    };
    let output_digest: Digest = output.digest();
    let od_words: [u32; 8] = output_digest.into();
    let mut od_bytes = Vec::new();
    for w in od_words {
        od_bytes.extend_from_slice(&w.to_le_bytes());
    }

    // user ELF (hand-written guest) + the official v1compat kernel -> ProgramBinary
    let user_elf = build_user_elf();
    let blob = risc0_binfmt::ProgramBinary::new(&user_elf, risc0_zkos_v1compat::V1COMPAT_ELF)
        .encode();
    println!("user elf bytes = {}", user_elf.len());
    println!("program binary bytes = {}", blob.len());

    // ExecutorEnv: stdin = write(serde u32) x2 + write_slice(digest 32B) = 40B
    let mut builder = ExecutorEnv::builder();
    builder.write(&a).unwrap();
    builder.write(&b).unwrap();
    builder.write_slice(&od_bytes);
    let env = builder.build().unwrap();

    let info = default_executor().execute(env, &blob).unwrap();

    let exit_str = match info.exit_code {
        ExitCode::Halted(code) => format!("Halted({code})"),
        other => format!("{other:?}"),
    };
    println!("exit = {exit_str}");
    println!("segments = {}", info.segments.len());
    let total_cycles: u64 = info.segments.iter().map(|s| s.cycles as u64).sum();
    println!("cycles total = {total_cycles}");
    for (i, s) in info.segments.iter().enumerate() {
        println!("segment[{i}] po2 = {} cycles = {}", s.po2, s.cycles);
    }

    let journal = &info.journal.bytes;
    println!("journal len = {}", journal.len());
    let mut words = Vec::new();
    for chunk in journal.chunks(4) {
        words.push(u32::from_le_bytes(chunk.try_into().unwrap()));
    }
    println!("journal words = {words:?}");
    println!("journal fnv1a = {:016x}", fnv1a(journal));
    println!("journal matches host precompute = {}", *journal == journal_want);

    let claim = info.receipt_claim.as_ref().unwrap();
    let claim_out: Digest = claim.output.digest();
    println!("claim output digest = {}", hex(&claim_out.as_bytes()));
    println!(
        "claim digest == host precompute = {}",
        claim_out == output_digest
    );

    assert_eq!(info.exit_code, ExitCode::Halted(0));
    assert_eq!(*journal, journal_want);
    assert_eq!(claim_out, output_digest);
    println!("risc0 execute-only ok");
}
