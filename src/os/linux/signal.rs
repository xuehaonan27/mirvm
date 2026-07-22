//! os::signal — Linux 信号原语：signal/sigaction 直通、sigaction 结构体
//! 副本改 handler 的读写、SEGV 排障 dump 安装。
//!
//! 原语不裁决：guest handler 白名单（sync 故障信号拒绝）、AS-trampoline
//! 物化、DFL/IGN 判定全部留引擎；本层只管诚实的内核调用与结构体布局知识。
//!
//! SEGV dump 的 ucontext 字段索引（gregs[16]=RIP、gregs[22]=CR2）是
//! Linux x86_64 知识——与 global_asm/asm-stub 的 x86_64 硬门同前提，记此。

/// SIG_DFL / SIG_IGN（sighandler_t 特殊值，按 usize 出入——leaf 类型纪律）。
pub const SIG_DFL: usize = libc::SIG_DFL;
pub const SIG_IGN: usize = libc::SIG_IGN;

// 同步故障信号号（引擎白名单裁决用）。
pub const SIGSEGV: i32 = libc::SIGSEGV;
pub const SIGBUS: i32 = libc::SIGBUS;
pub const SIGFPE: i32 = libc::SIGFPE;
pub const SIGILL: i32 = libc::SIGILL;
pub const SIGTRAP: i32 = libc::SIGTRAP;

/// signal(2) 直通：装 handler，返回旧 handler（均按 usize）。
pub fn signal(signum: i32, handler: usize) -> usize {
    unsafe { libc::signal(signum, handler as libc::sighandler_t) as usize }
}

/// sigaction 结构体（布局知识封装；引擎以副本改 handler 后回写内核，
/// 原 guest 结构不动——guest 可能复用/读回）。
pub struct Sigaction(libc::sigaction);

impl Sigaction {
    /// 从 guest 侧 act 指针复制一份（指针为 0 → None，与内核 act=NULL 语义对应）。
    ///
    /// # Safety
    /// ptr 非 0 时必须指向 guest 地址空间中一个完整 sigaction 结构（真实地址模型）。
    pub unsafe fn copy_from(ptr: u64) -> Option<Self> {
        if ptr == 0 {
            return None;
        }
        Some(Sigaction(unsafe { *(ptr as *const libc::sigaction) }))
    }

    pub fn handler(&self) -> usize {
        self.0.sa_sigaction
    }

    pub fn set_handler(&mut self, handler: usize) {
        self.0.sa_sigaction = handler;
    }
}

/// sigaction(2) 直通：act = 改好的副本（None = 内核 NULL），oldact 按地址回写。
pub fn sigaction(signum: i32, act: Option<&Sigaction>, oldact: u64) -> i32 {
    let act_ptr = act.map_or(std::ptr::null(), |p| &p.0 as *const libc::sigaction);
    unsafe { libc::sigaction(signum, act_ptr, oldact as *mut libc::sigaction) }
}

/// MIRVM_SEGV_DUMP 排障钩：安装 SA_SIGINFO 处理器——打印 fault RIP
/// （ucontext RIP 字段，x86_64 = gregs[REG_RIP=16]）、fault 地址（CR2=gregs[22]）
/// 与两者的 /proc/self/maps 归属，并把所属可执行段整段落 /tmp/mirvm-jitdump.bin
/// （可 objdump 反汇编找崩点），随后退出。
pub fn install_segv_dump() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = segv_dump_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
    }
}

unsafe extern "C" fn segv_dump_handler(
    _sig: i32,
    _info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    unsafe {
        let uc = ctx as *mut libc::ucontext_t;
        let rip = (*uc).uc_mcontext.gregs[16] as usize; // RIP
        let addr = (*uc).uc_mcontext.gregs[22] as usize; // CR2（真 fault 地址）
        eprintln!("mirvm-segv-dump: fault addr(CR2)={addr:#x} rip={rip:#x}");
        if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
            for line in maps.lines() {
                let start =
                    usize::from_str_radix(line.split('-').next().unwrap_or("0"), 16).unwrap_or(0);
                let end = usize::from_str_radix(
                    line.split_whitespace()
                        .nth(0)
                        .unwrap_or("0-0")
                        .split('-')
                        .nth(1)
                        .unwrap_or("0"),
                    16,
                )
                .unwrap_or(0);
                if addr >= start && addr < end {
                    eprintln!("mirvm-segv-dump: fault 归属: {line}");
                }
                if rip >= start && rip < end {
                    eprintln!("mirvm-segv-dump: rip 归属: {line}");
                    if line.contains("xp") {
                        let bytes = std::slice::from_raw_parts(start as *const u8, end - start);
                        let _ = std::fs::write("/tmp/mirvm-jitdump.bin", bytes);
                        eprintln!(
                            "mirvm-segv-dump: 可执行段已落 /tmp/mirvm-jitdump.bin（基址 {start:#x}）"
                        );
                    }
                }
            }
        }
        std::process::exit(134);
    }
}
