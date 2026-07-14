//! 冻结区：statics / 常量池 / fn-ptr 条目的真地址存储（M4.1 第 4 步）。
//!
//! 加载相物化（两遍法：先分后填破指针环，见 lower），发布后随 Module 只读共享——
//! 例外是 static mut / 内部可变性：guest 可写（真实地址裸写，引擎不经手）。
//! mmap RW 定容（GuestMemory/ByteRegion 同款）：地址终身稳定，重定位一次成真。
//!
//! M6 片2（D9b/D9c，L2 缓存）：**固定基址**。冻结区内的绝对地址（fn 条目、statics
//! 互指、字节码内嵌 const、fn_addrs 键）跨进程稳定的前提是区基址稳定——JVM CDS 同
//! 思路：映射到偏好地址，被占则响亮回退动态基址（本进程照常运行，仅不可缓存）。
//! 快照 = used 前缀字节；恢复 = 固定基址重映射 + memcpy（必须在 guest 运行前，
//! 快照语义 = lower 刚完成的洁净态；argv 等运行期输入不入快照，见 Module::finalize_entry_argv）。

/// 冻结区容量（虚拟保留；触碰才占物理页）。
const FROZEN_CAP: usize = 256 << 20;

/// 固定基址选址：PIE 映像/brk 随机化上界 ~0x66xx_xxxx_xxxx（mmap_rnd_bits=28），
/// mmap 自顶向下区在 0x7fxx_xxxx_xxxx 附近——0x6800_0000_0000 落在两带之间的空洞，
/// 与影子 IP（FUNC_IP_BASE，非规范高位、从不映射）无交集。
/// MAP_FIXED_NOREPLACE：被占 = EEXIST，绝不覆盖既有映射。
const FROZEN_FIXED_BASE: usize = 0x6800_0000_0000;

pub struct FrozenArena {
    base: *mut u8,
    used: usize,
    at_fixed_base: bool,
}

impl Default for FrozenArena {
    fn default() -> Self {
        Self::new()
    }
}

impl FrozenArena {
    fn map(addr: usize, flags_extra: libc::c_int) -> *mut libc::c_void {
        unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                FROZEN_CAP,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | flags_extra,
                -1,
                0,
            )
        }
    }

    pub fn new() -> Self {
        // 先试固定基址（L2 缓存可用的前提）；被占（并发单测/罕见 ASLR 冲突）则
        // 回退动态基址——语义不变，仅本进程产出不可序列化。
        let fixed = Self::map(FROZEN_FIXED_BASE, libc::MAP_FIXED_NOREPLACE);
        if fixed != libc::MAP_FAILED {
            return FrozenArena {
                base: fixed as *mut u8,
                used: 0,
                at_fixed_base: true,
            };
        }
        let base = Self::map(0, 0);
        assert!(base != libc::MAP_FAILED, "FrozenArena: mmap 失败");
        FrozenArena {
            base: base as *mut u8,
            used: 0,
            at_fixed_base: false,
        }
    }

    /// 从快照恢复（L2 warm 路径）。固定基址被占即 Err——调用方按缓存 miss 处理，
    /// 绝不在其他基址上重放快照（快照内嵌绝对地址，错基址 = 静默错值）。
    pub fn restore(snapshot: &[u8]) -> Result<Self, String> {
        assert!(snapshot.len() <= FROZEN_CAP, "冻结区快照超容量");
        let fixed = Self::map(FROZEN_FIXED_BASE, libc::MAP_FIXED_NOREPLACE);
        if fixed == libc::MAP_FAILED {
            return Err("冻结区固定基址被占，无法恢复快照".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(snapshot.as_ptr(), fixed as *mut u8, snapshot.len());
        }
        Ok(FrozenArena {
            base: fixed as *mut u8,
            used: snapshot.len(),
            at_fixed_base: true,
        })
    }

    /// 是否落在固定基址（false ⇒ 本区地址跨进程不稳定，禁止序列化）。
    pub fn at_fixed_base(&self) -> bool {
        self.at_fixed_base
    }

    /// 快照 = used 前缀（洁净态责任在调用方：guest 运行前拍）。
    pub fn snapshot(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base, self.used) }
    }

    /// bump 分配（按 align 对齐、清零），返回真地址。
    pub fn alloc(&mut self, size: u64, align: u64) -> u64 {
        let align = align.max(1) as usize;
        let aligned = (self.base as usize + self.used + align - 1) & !(align - 1);
        let start = aligned - self.base as usize;
        let end = start + size as usize;
        assert!(
            end <= FROZEN_CAP,
            "FrozenArena: 冻结区耗尽（{} MiB）",
            FROZEN_CAP >> 20
        );
        self.used = end;
        aligned as u64
    }
}

impl Drop for FrozenArena {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, FROZEN_CAP) };
    }
}

impl std::fmt::Debug for FrozenArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FrozenArena {{ base: {:p}, used: {}, fixed: {} }}",
            self.base, self.used, self.at_fixed_base
        )
    }
}

// L2 序列化（M6 片2）：非固定基址的区含跨进程不稳定地址——序列化必须失败
// （上层按"本次不缓存"处理），绝不产出会静默错值的快照。
impl serde::Serialize for FrozenArena {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.at_fixed_base {
            return Err(serde::ser::Error::custom("冻结区不在固定基址，不可序列化"));
        }
        serializer.serialize_bytes(self.snapshot())
    }
}

impl<'de> serde::Deserialize<'de> for FrozenArena {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // postcard bytes = 借用切片可用；用 &[u8] 承接避免中间拷贝
        let bytes: &[u8] = serde::Deserialize::deserialize(deserializer)?;
        FrozenArena::restore(bytes).map_err(serde::de::Error::custom)
    }
}

// SAFETY: 发布后只读（static mut 的 guest 写走裸地址，不经 &self）；
// 区随 Module 生命周期共享给各执行线程（C8 发布后只读纪律）。
unsafe impl Send for FrozenArena {}
unsafe impl Sync for FrozenArena {}

#[cfg(test)]
mod tests {
    use super::FrozenArena;

    /// 固定基址快照/恢复往返：地址值稳定、内容逐字节保真、恢复后可继续分配。
    #[test]
    fn snapshot_restore_roundtrip_preserves_addresses_and_bytes() {
        let mut a = FrozenArena::new();
        if !a.at_fixed_base() {
            // 并发单测抢占了固定基址——本测试需要独占，让位（其余断言无意义）
            eprintln!("skip: 固定基址被占");
            return;
        }
        let p = a.alloc(16, 8);
        let q = a.alloc(9, 1);
        unsafe {
            (p as *mut u64).write(0xdead_beef_cafe_f00d);
            // 冻结区典型形态：内嵌指向区内的绝对指针
            ((p + 8) as *mut u64).write(q);
            std::ptr::copy_nonoverlapping(
                c"mirvm-l2".to_bytes_with_nul().as_ptr(),
                q as *mut u8,
                9,
            );
        }
        let snap = a.snapshot().to_vec();
        drop(a);

        let b = FrozenArena::restore(&snap).expect("恢复失败");
        assert!(b.at_fixed_base());
        unsafe {
            assert_eq!((p as *const u64).read(), 0xdead_beef_cafe_f00d);
            let q2 = ((p + 8) as *const u64).read();
            assert_eq!(q2, q, "内嵌绝对指针必须逐位稳定");
            assert_eq!(
                std::slice::from_raw_parts(q2 as *const u8, 9),
                b"mirvm-l2\0"
            );
        }
        // 恢复后追加分配（argv 终结化走此路径）
        let mut b = b;
        let r = b.alloc(8, 8);
        assert!(r >= q + 9, "追加分配必须落在快照之后");
    }
}
