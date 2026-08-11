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

/// 固定基址数值与白名单判据统归 `super::addrlayout`（共享常量层）；
/// 选址论证与域模型见其模块头。
use super::addrlayout::{BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR, image_addr, is_valid_home};

pub struct FrozenArena {
    base: *mut u8,
    used: usize,
    at_fixed_base: bool,
    /// 本区所属域的固定基址（serde 自描述用；动态回退时仍记原意向域）。
    home: usize,
}

impl Default for FrozenArena {
    fn default() -> Self {
        Self::new()
    }
}

impl FrozenArena {
    fn new_at(home: usize) -> Self {
        // 先试本域固定基址（缓存可用的前提）；被占（并发单测/罕见 ASLR 冲突）则
        // 回退动态基址——语义不变，仅本进程产出不可序列化。
        if let Some(p) =
            crate::os::mem::map_fixed_preferred(home, FROZEN_CAP, crate::os::mem::Prot::RW)
        {
            return FrozenArena {
                base: p,
                used: 0,
                at_fixed_base: true,
                home,
            };
        }
        let base = crate::os::mem::map_anon(FROZEN_CAP, crate::os::mem::Prot::RW, false);
        assert!(!base.is_null(), "FrozenArena: mmap 失败");
        FrozenArena {
            base,
            used: 0,
            at_fixed_base: false,
            home,
        }
    }

    /// 程序模块（delta；无底座时=全量模块）的冻结区。
    pub fn new() -> Self {
        Self::new_at(DELTA_FIXED_ADDR)
    }

    /// 底座构建会话专用（S4）。
    pub fn new_base_image() -> Self {
        Self::new_at(BASE_IMAGE_FIXED_ADDR)
    }

    /// 依赖 image 构建会话专用（S3′）：落第 k 个样条域。
    pub fn new_image(k: usize) -> Self {
        Self::new_at(image_addr(k))
    }

    /// 从快照恢复到指定域（L2 warm / 底座 / 依赖 image 装载）。固定基址被占即 Err——
    /// 调用方按缓存 miss 处理，绝不在其他基址上重放快照（快照内嵌绝对地址，错基址=静默错值）。
    pub fn restore(snapshot: &[u8], home: usize) -> Result<Self, String> {
        assert!(snapshot.len() <= FROZEN_CAP, "冻结区快照超容量");
        assert!(is_valid_home(home), "冻结区恢复域非法: {home:#x}");
        let Some(p) =
            crate::os::mem::map_fixed_preferred(home, FROZEN_CAP, crate::os::mem::Prot::RW)
        else {
            return Err(format!("冻结区固定基址 {home:#x} 被占，无法恢复快照"));
        };
        unsafe {
            std::ptr::copy_nonoverlapping(snapshot.as_ptr(), p, snapshot.len());
        }
        Ok(FrozenArena {
            base: p,
            used: snapshot.len(),
            at_fixed_base: true,
            home,
        })
    }

    /// 是否落在固定基址（false ⇒ 本区地址跨进程不稳定，禁止序列化）。
    pub fn at_fixed_base(&self) -> bool {
        self.at_fixed_base
    }

    /// 本区所属域的固定基址（S4：装载方核对"底座真的在底座域"）。
    pub fn home(&self) -> usize {
        self.home
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
        unsafe { crate::os::mem::unmap(self.base, FROZEN_CAP) };
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

// L2/底座序列化：非固定基址的区含跨进程不稳定地址——序列化必须失败
// （上层按"本次不缓存"处理），绝不产出会静默错值的快照。
// S4 起**自描述**：(home, bytes) 元组——反序列化恢复到快照自带的域，
// 域白名单在 restore 里断言（防伪造快照把区放到任意地址）。
impl serde::Serialize for FrozenArena {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.at_fixed_base {
            return Err(serde::ser::Error::custom("冻结区不在固定基址，不可序列化"));
        }
        use serde::ser::SerializeTuple;
        let mut t = serializer.serialize_tuple(2)?;
        t.serialize_element(&(self.home as u64))?;
        t.serialize_element(&serde_bytes_shim::Bytes(self.snapshot()))?;
        t.end()
    }
}

impl<'de> serde::Deserialize<'de> for FrozenArena {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // postcard bytes = 借用切片可用；用 &[u8] 承接避免中间拷贝
        let (home, bytes): (u64, &[u8]) = serde::Deserialize::deserialize(deserializer)?;
        let home = usize::try_from(home).map_err(serde::de::Error::custom)?;
        if !is_valid_home(home) {
            return Err(serde::de::Error::custom(format!(
                "冻结区快照域非法: {home:#x}"
            )));
        }
        FrozenArena::restore(bytes, home).map_err(serde::de::Error::custom)
    }
}

/// serialize_bytes 的元组内嵌形态：Serialize for &[u8] 走序列 u8 编码（postcard 下
/// 逐字节 varint，体积/速度都劣化）——包一层强制 bytes 通道。
mod serde_bytes_shim {
    pub struct Bytes<'a>(pub &'a [u8]);
    impl serde::Serialize for Bytes<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }
}

// SAFETY: 发布后只读（static mut 的 guest 写走裸地址，不经 &self）；
// 区随 Module 生命周期共享给各执行线程（C8 发布后只读纪律）。
unsafe impl Send for FrozenArena {}
unsafe impl Sync for FrozenArena {}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::super::addrlayout::{
        BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR, IMAGE_SPLINE_BASE, IMAGE_SPLINE_COUNT,
        IMAGE_SPLINE_STEP, image_addr, is_valid_home,
    };
    use super::FrozenArena;

    static FIXED_ADDRESS_TEST: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// S3′ 样条域白名单：底座/delta/对齐样条合法，越界/未对齐/杂散非法。
    #[test]
    fn image_spline_home_validation() {
        assert!(is_valid_home(BASE_IMAGE_FIXED_ADDR));
        assert!(is_valid_home(DELTA_FIXED_ADDR));
        assert!(is_valid_home(image_addr(0)));
        assert!(is_valid_home(image_addr(1)));
        assert!(is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1)));
        // 未对齐（样条中点）非法
        assert!(!is_valid_home(IMAGE_SPLINE_BASE + IMAGE_SPLINE_STEP / 2));
        // 越界非法
        assert!(!is_valid_home(
            IMAGE_SPLINE_BASE + IMAGE_SPLINE_COUNT * IMAGE_SPLINE_STEP
        ));
        // 杂散地址非法（伪造快照防线）
        assert!(!is_valid_home(0x1234_5678));
        assert!(!is_valid_home(0x7f00_0000_0000));
        // 样条不触 mmap 自顶向下带
        assert!(image_addr(IMAGE_SPLINE_COUNT - 1) < 0x7f00_0000_0000);
        // 各域两两不交叠（FROZEN_CAP 远小于步距）
        assert!(image_addr(0) > DELTA_FIXED_ADDR);
        assert!(image_addr(1) - image_addr(0) == IMAGE_SPLINE_STEP);
    }

    /// 固定基址快照/恢复往返：地址值稳定、内容逐字节保真、恢复后可继续分配。
    #[test]
    fn snapshot_restore_roundtrip_preserves_addresses_and_bytes() {
        let _fixed_address = FIXED_ADDRESS_TEST.lock().unwrap();
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

        let b = FrozenArena::restore(&snap, DELTA_FIXED_ADDR).expect("恢复失败");
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

    /// S4 双域：底座区与 delta 区同时在场，跨域绝对指针（delta→base 方向，
    /// 底座查找命中后的常见形态）恢复后逐位稳定。
    #[test]
    fn dual_domain_arenas_coexist_and_cross_references_survive_restore() {
        let _fixed_address = FIXED_ADDRESS_TEST.lock().unwrap();
        let mut base = FrozenArena::new_base_image();
        let mut delta = FrozenArena::new();
        if !base.at_fixed_base() || !delta.at_fixed_base() {
            eprintln!("skip: 固定基址被占");
            return;
        }
        assert_eq!(
            (base.alloc(8, 8) & !0xffff_ffff) as usize,
            BASE_IMAGE_FIXED_ADDR
        );
        let b_cell = base.alloc(8, 8);
        unsafe { (b_cell as *mut u64).write(0x42) };
        // delta 内嵌指向底座的绝对指针（fn 条目/静态去重的形态）
        let d_ptr = delta.alloc(8, 8);
        unsafe { (d_ptr as *mut u64).write(b_cell) };

        let base_snap = base.snapshot().to_vec();
        let delta_snap = delta.snapshot().to_vec();
        drop(delta);
        drop(base);

        let _base2 = FrozenArena::restore(&base_snap, BASE_IMAGE_FIXED_ADDR).expect("底座恢复");
        let _delta2 = FrozenArena::restore(&delta_snap, DELTA_FIXED_ADDR).expect("delta 恢复");
        unsafe {
            let cross = (d_ptr as *const u64).read();
            assert_eq!(cross, b_cell, "跨域指针逐位稳定");
            assert_eq!((cross as *const u64).read(), 0x42, "经跨域指针可读底座内容");
        }
    }
}
