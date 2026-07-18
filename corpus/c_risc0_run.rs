#!/usr/bin/env mirvm
---
[dependencies]
risc0-zkvm = { version = "=3.0.6", default-features = false, features = ["prove"] }
risc0-binfmt = "=3.0.5"
risc0-zkos-v1compat = "=2.2.3"
---
// 【expected-red】risc0-zkvm 3.0.6（crates.io 2026-07-18 最新稳定线；
// 5.0.0-rc.1 仍是预发布）zkVM **execute-only** 差分：手写 rv32im guest 在
// ExecutorEnv 里经 default_executor 执行，取 journal 字节打 FNV + 公开值
// 断言。全程不证明。driver 完整可用（B 维 native 13 行 oracle 固定于文末），
// A/C 两维撞同一引擎边界（见文末诊断链），边界闭合后应原样 XPASS 翻绿。
//
// 形态与钉版本（三条全有实勘证据）：
//   * features=["prove"] 而非 execute-only 直觉的 ["std"]——3.0.6 起进程内
//     执行器被 feature 门控成两级：default_executor() 在无 prove 时返回
//     ExternalProver("ipc", r0vm 路径)（src/host/client/prove/mod.rs:230
//     default_executor 函数体），即外发 r0vm 子进程；r0vm 二进制本机缺席
//     （红因④形态），且子进程 IPC 非本 driver 语义面。prove 打开后
//     default_executor → LocalProver::execute → ExecutorImpl::from_elf
//     进程内执行。prover 从未被调用（无 Receipt、无证明、无 Fiat-Shamir
//     随机通道），"execute-only 不证明"成立。依赖闭包：247 normal crate
//     （cargo tree 实数）；三个 -sys crate 用 cc/g++ 编 C++ kernels（g++
//     在场，无 cmake/bindgen/clang/nasm 需求）；native dev 冷构建 2m03s
//     （8 核），预算内。
//   * risc0-zkvm-methods（官方预构建 guest ELF crate）在 crates.io **未发布**
//     （API 404 实勘），risc0-build 路径又需 cargo-risczero 工具链（缺席）。
//     按批任务文本明确授权的"手写"路线：driver 内置 rv32im 迷你汇编器
//     （定宽 li 两遍定址），现场汇编 guest 机器码并手工封最小 ELF32
//     （EM_RISCV/ET_EXEC/单 PT_LOAD），无任何外部工具链。
//   * 3.0.6 起 execute() 吃的不是裸 ELF 而是 ProgramBinary 容器（user ELF +
//     kernel ELF；ExecutorImpl::from_elf → ProgramBinary::decode 校验
//     AbiKind::V1Compat/^1.0.0）。kernel = risc0-zkos-v1compat 2.2.3 crate
//     内嵌的预构建 V1COMPAT_ELF（include_bytes! 官方件，syscall v1 兼容
//     层）。risc0-binfmt/risc0-zkos-v1compat 钉到 risc0-zkvm 3.0.6 依赖
//     要求锁定的确切版本（^3.0.5/=3.0.5、^2.2.3/=2.2.3），ProgramBinary
//     ::new().encode() 为公开 API。
//
// 测试面（单 driver 全覆盖）：
//   ① ExecutorEnv 双写入通道：write(&u32)（risc0 serde to_vec → 1 word LE）
//     ×2 + write_slice(32B digest)，stdin 流共 40B。
//   ② guest sys_read(fd=STDIN) 三次：读输入 8B、读 digest 32B、EOF 探针
//     （请求 4B 必须返回 0）——nread 返回值逐次校验，异常即跳 fail。
//   ③ guest 内部乘法/断言：p = 37*41（M 扩展 mul），bne 断言 p==1517；
//     q = p*7+13。失败路径：journal 写 0xDEADBEEF 标记 + exit code 1
//     （区别于正常路径，三维 diff 必捕）。
//   ④ env::commit 公开输出：sys_write(fd=JOURNAL, [a,b,p,q] 16B)。
//   ⑤ sys_halt(user_exit=0, output_digest)：executor.rs:258 实证——halt
//     digest 为零则 journal 被丢弃（SessionInfo.journal=None），故 digest
//     必须非零且本 driver 做到逐比特正确：host 侧用 risc0 自身类型预算
//     Output{journal: Pruned(SHA-256(journal)), assumptions: Pruned(ZERO)}
//     .digest()（= guest env::exit 的 finalize 算式，ASSUMPTIONS_DIGEST
//     初值 Pruned(Digest::ZERO)），经 stdin 交给 guest 透传。打印
//     receipt_claim.output.digest() 验证回读一致——digest 真实流经
//     guest halt 寄存器的确证。
//   ⑥ 会话形状：segments=1、po2、user cycles、ProgramBinary 字节数全打印
//     （纯执行器的确定函数，三维互锚）。
//
// 确定性纪律：输入全定值；guest 机器码由定值汇编器产出；无时间/随机/
// 线程序（LocalProver::execute 单线程路径，rayon 只在证明面）/env 依赖；
// 未注册 tracing subscriber（risc0 的 tracing 事件全静默丢弃）→ stderr
// 真空；错误路径一律 panic 不打印。输出 13 行。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_risc0_run.rs
//   B: d=$(grep -l 'name = "c_risc0_run"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_risc0_run.rs
//
// 调研期记录在案的 ABI 事实源（driver 全部按此写就）：SOFTWARE ecall 寄存
// 器约定 t0=2/t6=nr/a0=buf/a1=len/a2=名串/a3..=args（impl_syscall! 宏体）；
// Syscall 编号 Read=12/Write=16；fileno STDIN=0/JOURNAL=3；sys_halt 的
// a0=TERMINATE|(user_exit<<8)、a1=OutDigest 指针（kernel.s _ecall_halt 读
// 8 words 进机器寄存器）；SyscallName = 指向 NUL 结尾名串的裸指针
// （"risc0_zkvm_platform::syscall::nr::SYS_READ"/"..._WRITE" 内嵌进 guest
// 数据段）；TEXT_START=0x0020_0800、KERNEL_START=0xC000_0000（user ELF
// 装载上限）、user entry 经 0x0001_0000 槽由 kernel 跳转；ProgramBinary
// 磁盘格式 = MAGIC+版本+postcard 头+user/kernel 长度前缀（encode() 包办）。
//
// 三维实测（2026-07-18）= **expected-red：B 绿 / A、C 同签名红**。
//
// 红因定类（② FFI/native-archive 符号边界的新姊妹形态，非 driver 问题）：
// risc0-zkvm 的三个 REQUIRED circuit crate（rv32im/recursion/keccak）经
// prove/default feature 各拉一个 `-sys` crate，其 build.rs **无条件**以 cc
// 编译 kernels/cxx/*.cpp 成静态归档（只设 CUDA 开关，无"关 C++"官方开关）。
// 三个 librisc0_{rv32im,recursion,keccak}_cpu.a 各导出同一 **weak** 符号
// `_ZdlPvS_`（C++ sized-delete COMDAT，nm 实测每档 W×1）。mirvm 物化期把
// 每档 .a 整体 .so 化并全量导出 → 两两 dynsym 可见同名 →
// reject_symbol_ambiguity 按设计拒绝（src/lower/mod.rs:2162 panic）。
// native 语义本无歧义：归档成员按需懒拉 + weak 符号首件胜出（B 维绿即
// 实证）。即 C2（符号在 rlib，已闭合）的姊妹票：**符号跨两个 native 归档
// 同名（weak/COMDAT）碰撞**——转正方向 = 物化期同名决议纳入 weak 语义
// （首件胜出并记档）或归档成员级懒拉模型。
//
// 无合法绕行（逐条实勘排除）：① -sys C++ 编译无 feature/env 关断
// （build.rs 唯一开关是 CUDA 叠加）；② risc0-zkvm 进程内执行器必走 prove
// feature（无 prove → default_executor 外发 r0vm 子进程，本机无此二进制），
// prove 又强带 circuit-*/prove（cargo 无负 feature）；③ 钉版本无逃逸——
// 2.3.2 同样 REQUIRED 依赖三个 circuit crate（同 -sys 拓扑），且碰撞在
// 引擎侧与上游版本无关，换旧 major 属裁剪 workload 非"避上游破洞"。
//
// 最小复现（/tmp 实证，同签名 exit 101）：cargo-script 仅依赖
// risc0-circuit-rv32im-sys =4.0.3 + risc0-circuit-recursion-sys =4.0.3，
// main 只打一行——物化期即撞同文 panic（`ecc02209….so` 哈希两跑一致，
// 系内容寻址缓存）。
//
// 接线建议：red_code=101（rustc 线程 panic 的标准退出码）；red_pattern=
// "静态归档导出符号 `_ZdlPvS_` 同时来自"（稳定子串；归档哈希随内容变、
// panic 头括号内为线程号逐进程变，均不可做全串匹配）。三维 stderr 在
// expected-red 下天然不逐字节（线程号），属锁定特征非失序。
//
// B 维 oracle（native，2m23s 冷构建，复跑 md5 自对拍稳定，13 行逐字节）：
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
// 引擎边界闭合后，A/C 应逐字节复现上列 13 行（XPASS 强制转绿）。
//
// 各维耗时实测：A = 3m08s 冷（mirvm 降低 247-crate 闭包后于物化点红；
// 终稿热复跑 0.76s 同签名）、B = 2m23s 冷 / ~10s 热、C = 0.75s（依赖镜像
// 缓存热，同签名红）。
// 依赖尺寸证据（预算铁律存档）：cargo tree normal 去重 247 crate、含
// build 依赖全量去重 324；三个 -sys 用 cc/g++（在场），无 cmake/bindgen/
// clang/nasm 需求；native dev 冷构建 2m03s（probe 同配置），构建量级在
// 20min 预算内——非"判不可"，系引擎红。
use risc0_zkvm::sha::{Digestible, Impl, Sha256};
use risc0_zkvm::{default_executor, Digest, ExecutorEnv, ExitCode, MaybePruned, Output};

// ---------------- rv32im 迷你汇编器（定宽 li，两遍定址） ----------------
const TEXT_START: u32 = 0x0020_0800; // risc0-zkvm-platform memory::TEXT_START

// 寄存器编号（risc0-zkvm-platform syscall::reg_abi）
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

// 由于 finish 需要分支寄存器，简化：分支立即记录寄存器
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

// ecall 号（risc0-zkvm-platform syscall::ecall）
const EC_HALT: u32 = 0;
const EC_SOFTWARE: u32 = 2;
// syscall 编号（Syscall enum）
const NR_READ: u32 = 12;
const NR_WRITE: u32 = 16;
// fd（fileno）
const FD_STDIN: u32 = 0;
const FD_JOURNAL: u32 = 3;

/// 汇编 guest 程序体。resolve: 数据符号 → 绝对地址。
fn assemble_body(resolve: &dyn Fn(&str) -> u32) -> Vec<u32> {
    let mut a = Asm2::new();
    // ---- sys_read(fd=0, INBUF, 8)：两个 u32 输入 ----
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
    // ---- sys_read(fd=0, DIGBUF, 32)：host 预算的 Output digest 8 words ----
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
    // ---- EOF 探针：sys_read(fd=0, OUTBUF, 4) 必须返回 0 ----
    a.li(T0, EC_SOFTWARE);
    a.li(T6, NR_READ);
    a.li_data(A0, "OUTBUF", resolve);
    a.li(A1, 4);
    a.li_data(A2, "NREAD", resolve);
    a.li(A3, FD_STDIN);
    a.li(A4, 4);
    a.emit(ECALL);
    a.bne(A0, X0, "fail");
    // ---- p = a * b；断言 p == 1517 ----
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
    // ---- sys_write(fd=JOURNAL, OUTBUF, 16) = env::commit 公开输出 ----
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
    a.emit(0); // 非法指令兜底（不可达）
    // ---- fail：journal 写 0xDEADBEEF 标记，exit code 1 ----
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

/// 两遍定址：先以 0 解析数据符号测代码长，再按布局重汇编。
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

    // 段内容 = 代码 + 对齐填充 + 数据区（INBUF/OUTBUF/DIGBUF 初值 0）+ 名串
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

    // 最小 ELF32（EM_RISCV / ET_EXEC / 单 PT_LOAD）
    let mut elf = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1, 1, 0]); // ident: ELF32 LE
    elf.extend_from_slice(&[0; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&243u16.to_le_bytes()); // e_machine = EM_RISCV
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&TEXT_START.to_le_bytes()); // e_entry
    elf.extend_from_slice(&52u32.to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags（rv32im，无 RVC）
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

// ---------------- host 侧 ----------------

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
    // 定值输入与期望（guest 内部独立重算并断言）
    let (a, b): (u32, u32) = (37, 41);
    let p: u32 = a * b; // 1517
    let q: u32 = p * 7 + 13; // 10632

    // 期望 journal = 4 个 u32 LE
    let mut journal_want = Vec::new();
    for w in [a, b, p, q] {
        journal_want.extend_from_slice(&w.to_le_bytes());
    }

    // host 用 risc0 自身类型预算 env::exit 的 Output digest：
    // journal_digest = SHA-256(journal)；assumptions 空 = Digest::ZERO。
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

    // user ELF（手写 guest）+ 官方 v1compat kernel → ProgramBinary 容器
    let user_elf = build_user_elf();
    let blob = risc0_binfmt::ProgramBinary::new(&user_elf, risc0_zkos_v1compat::V1COMPAT_ELF)
        .encode();
    println!("user elf bytes = {}", user_elf.len());
    println!("program binary bytes = {}", blob.len());

    // ExecutorEnv：stdin = write(serde u32)×2 + write_slice(digest 32B) = 40B
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
