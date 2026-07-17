//! 条目 stub 代码域（P1，decision-history §7.6）：fn-ptr 值可执行化。
//!
//! 与冻结区三域同族的固定基 mmap：stub 偏移稳定 ⇒ 字节码/冻结字节可烤 stub
//! 地址（值域跨进程稳定）。stub 内容（`movabs rax, <closure 码址>; jmp rax`）
//! 含每进程随机的 closure 地址——**不进快照**：启动相按模块里的 sites 配方
//! 重建字节（asm_sites/GOT 同契约），填完整域 mprotect RX（W^X）。
//! 域被占 = 响亮失败（错基址 = 跳崖）；冷路径动态回退则本进程可用但不可缓存
//! （FrozenArena 同款纪律）。

/// stub 间距（16B：movabs rax 10B + jmp rax 2B = 最长 12B，按 16 对齐）
pub const STUB_STRIDE: u64 = 16;

/// 代码域容量（虚拟保留；条目 stub 每实例一条，64 MiB >> 任何真实负载）
const CODE_CAP: usize = 64 << 20;

/// 第三固定地址域族（代码）：delta 0x6C00 / 底座 0x6D00 / image 样条 0x6E00+k。
/// 与冻结区 0x68/0x69/0x6A00 三域同构、互不相交；上界不触 mmap 自顶向下带。
pub const DELTA_CODE_ADDR: usize = 0x6C00_0000_0000;
pub const BASE_CODE_ADDR: usize = 0x6D00_0000_0000;
pub const IMAGE_CODE_SPLINE: usize = 0x6E00_0000_0000;
pub const IMAGE_CODE_STEP: usize = 1 << 34;
pub const IMAGE_CODE_COUNT: usize = 1300;

/// 第 k 个依赖 image 的代码域基址。
pub fn image_code_addr(k: usize) -> usize {
    assert!(k < IMAGE_CODE_COUNT, "image 代码样条越界: k={k}");
    IMAGE_CODE_SPLINE + k * IMAGE_CODE_STEP
}

/// 冻结域基址 → 本模块的代码域基址（同一 k 的不变量：delta↔delta、底座↔底座、
/// image_spline(k)↔image_code(k)）。非冻结白名单基址 ⇒ None（引擎不变量违规）。
pub fn code_home_for_frozen(home: usize) -> Option<usize> {
    use super::frozen::{
        BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR, IMAGE_SPLINE_BASE, IMAGE_SPLINE_STEP,
    };
    match home {
        DELTA_FIXED_ADDR => Some(DELTA_CODE_ADDR),
        BASE_IMAGE_FIXED_ADDR => Some(BASE_CODE_ADDR),
        h if h >= IMAGE_SPLINE_BASE
            && (h - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < super::frozen::IMAGE_SPLINE_COUNT =>
        {
            Some(image_code_addr((h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP))
        }
        _ => None,
    }
}

/// 合法代码域白名单（serde 配方回放与装载防御双验证；伪造快照防线）。
pub fn is_valid_code_home(addr: usize) -> bool {
    addr == DELTA_CODE_ADDR
        || addr == BASE_CODE_ADDR
        || (addr >= IMAGE_CODE_SPLINE
            && (addr - IMAGE_CODE_SPLINE).is_multiple_of(IMAGE_CODE_STEP)
            && (addr - IMAGE_CODE_SPLINE) / IMAGE_CODE_STEP < IMAGE_CODE_COUNT)
}

/// 值是否落在任一 stub 代码域带（P1：FFI 可派生条目的 fn-ptr 值——本身已是
/// 可执行码址；逃逸物化点据此跳过二次包装，decision-history §7.6 项5）
pub fn is_stub_addr(v: u64) -> bool {
    let a = v as usize;
    if a >= DELTA_CODE_ADDR && a < DELTA_CODE_ADDR + CODE_CAP {
        return true;
    }
    if a >= BASE_CODE_ADDR && a < BASE_CODE_ADDR + CODE_CAP {
        return true;
    }
    if a >= IMAGE_CODE_SPLINE {
        let off = a - IMAGE_CODE_SPLINE;
        return off / IMAGE_CODE_STEP < IMAGE_CODE_COUNT && off % IMAGE_CODE_STEP < CODE_CAP;
    }
    false
}

/// stub 字节：`movabs rax, target; jmp rax`（48 B8 <imm64> FF E0），12B 实长。
fn emit_stub_bytes(target: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = 0x48;
    b[1] = 0xb8;
    b[2..10].copy_from_slice(&target.to_le_bytes());
    b[10] = 0xff;
    b[11] = 0xe0;
    b
}

/// 单地址域的 stub 区（本模块域一份；image 域随 absorb 另挂）。
pub struct StubArena {
    base: *mut u8,
    used: usize,
    at_fixed_base: bool,
    home: usize,
}

impl Default for StubArena {
    /// serde skip 的空占位（未映射；启动相按 sites 配方重建时替换）
    fn default() -> Self {
        StubArena {
            base: std::ptr::null_mut(),
            used: 0,
            at_fixed_base: false,
            home: 0,
        }
    }
}

impl StubArena {
    fn map(addr: usize, flags_extra: libc::c_int) -> *mut libc::c_void {
        unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                CODE_CAP,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | flags_extra,
                -1,
                0,
            )
        }
    }

    /// 固定基物化（lower 冷路径与启动相重建共用选址纪律）：
    /// 先试本域固定基址（可缓存前提）；被占回退动态基址——语义不变，
    /// 仅本进程产出不可序列化（FrozenArena 同款）。
    pub fn new_at(home: usize) -> Self {
        let fixed = Self::map(home, libc::MAP_FIXED_NOREPLACE);
        if fixed != libc::MAP_FAILED {
            return StubArena {
                base: fixed as *mut u8,
                used: 0,
                at_fixed_base: true,
                home,
            };
        }
        let base = Self::map(0, 0);
        assert!(base != libc::MAP_FAILED, "StubArena: mmap 失败");
        StubArena {
            base: base as *mut u8,
            used: 0,
            at_fixed_base: false,
            home,
        }
    }

    /// 严格装载（warm/image 回放）：固定基被占即 Err——调用方按 cache miss
    /// 处理（字节码烤了此域 stub 地址，错基址重放 = 跳崖）。
    pub fn map_fixed(home: usize) -> Result<Self, String> {
        assert!(
            is_valid_code_home(home),
            "StubArena 恢复域非法: {home:#x}"
        );
        let fixed = Self::map(home, libc::MAP_FIXED_NOREPLACE);
        if fixed == libc::MAP_FAILED {
            return Err(format!("stub 代码域固定基址 {home:#x} 被占"));
        }
        Ok(StubArena {
            base: fixed as *mut u8,
            used: 0,
            at_fixed_base: true,
            home,
        })
    }

    pub fn new() -> Self {
        Self::new_at(DELTA_CODE_ADDR)
    }

    pub fn new_base_image() -> Self {
        Self::new_at(BASE_CODE_ADDR)
    }

    pub fn new_image(k: usize) -> Self {
        Self::new_at(image_code_addr(k))
    }

    /// 第 idx 个 stub 位的地址（不 bump——lower 按配方位序先算地址烤值，
    /// 启动相物化器按同序 alloc_stub 复现同址）
    pub fn addr_of(&self, idx: u64) -> u64 {
        self.base as usize as u64 + idx * STUB_STRIDE
    }

    /// 分一个 stub 位（bump，STUB_STRIDE 对齐），返回真地址；字节由启动相填。
    pub fn alloc_stub(&mut self) -> u64 {
        let addr = self.base as usize + self.used;
        self.used += STUB_STRIDE as usize;
        assert!(
            self.used <= CODE_CAP,
            "StubArena: 代码域耗尽（{} MiB）",
            CODE_CAP >> 20
        );
        addr as u64
    }

    /// 启动相填字节：`movabs rax, target; jmp rax`。addr 必须出自本区 alloc_stub。
    pub fn write_stub(&self, addr: u64, target: u64) {
        let bytes = emit_stub_bytes(target);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len())
        };
    }

    /// 填完封存：整域 RX（W^X）。map 时用过的写权限到此为止。
    pub fn seal(&self) {
        if self.base.is_null() || self.used == 0 {
            return;
        }
        let rc = unsafe {
            libc::mprotect(
                self.base as *mut libc::c_void,
                CODE_CAP,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        };
        assert!(rc == 0, "StubArena: mprotect RX 失败 rc={rc}");
    }

    pub fn at_fixed_base(&self) -> bool {
        self.at_fixed_base
    }

    pub fn home(&self) -> usize {
        self.home
    }

    /// 快照/跳过判定的可序列化前提：有 sites 的模块若代码域不在固定基址，
    /// stub 地址跨进程不稳定——与冻结区同规则拒缓存。
    pub fn is_mapped(&self) -> bool {
        !self.base.is_null()
    }
}

impl Drop for StubArena {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe { libc::munmap(self.base as *mut libc::c_void, CODE_CAP) };
        }
    }
}

impl std::fmt::Debug for StubArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "StubArena {{ base: {:p}, used: {}, fixed: {} }}",
            self.base, self.used, self.at_fixed_base
        )
    }
}

// SAFETY: 启动相填满后封存只读可执行；与 Module 同寿命共享给各执行线程
// （FrozenArena 同款发布后只读纪律）。
unsafe impl Send for StubArena {}
unsafe impl Sync for StubArena {}
