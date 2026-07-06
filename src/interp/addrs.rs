//! 地址表：真实宿主地址模式（账本 C2；Miri native-lib 同构）。
//!
//! 每个分配的基址 = 其宿主缓冲的真实地址（MirvmAllocBytes 保证对齐与唯一性）。
//! 本表维护 AllocId ↔ 基址双向映射：正向查询给 adjust_alloc_root_pointer/ptr_get_alloc，
//! 反向有序表给 wildcard（int2ptr 来源）指针解引用。
//!
//! 全局分配的"预分配"机制：地址分配时先建零缓冲（内容后到），材料化时
//! adjust_global_allocation 从 `prepared` 取走同一块缓冲再拷入 tcx 字节——
//! 这保证"先要地址后要内容"和指针环（A→B→A）都能收敛。

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::AllocId;

use super::alloc_bytes::MirvmAllocBytes;

#[derive(Debug, Default)]
pub struct AddrTable {
    base: FxHashMap<AllocId, u64>,
    /// 按基址排序的 (base, id)；死分配移除（基址映射保留供悬垂指针算偏移）。
    /// 真实地址乱序到来，插入走二分。地址 0（TypeId 哨兵）不入此表。
    by_addr: Vec<(u64, AllocId)>,
    /// 已定地址、尚未材料化的全局分配缓冲。
    pub prepared: FxHashMap<AllocId, MirvmAllocBytes>,
}

impl AddrTable {
    pub fn base_of(&self, id: AllocId) -> Option<u64> {
        self.base.get(&id).copied()
    }

    /// 登记分配的真实基址。
    pub fn register(&mut self, id: AllocId, addr: u64) {
        let old = self.base.insert(id, addr);
        debug_assert!(old.is_none_or(|o| o == addr), "分配 {id:?} 的基址被改写");
        if addr == 0 {
            return; // TypeId 哨兵
        }
        match self.by_addr.binary_search_by_key(&addr, |&(a, _)| a) {
            Err(pos) => self.by_addr.insert(pos, (addr, id)),
            // 同址活分配不可能（宿主分配器保证唯一）；防御性覆盖
            Ok(pos) => self.by_addr[pos] = (addr, id),
        }
    }

    /// wildcard 指针（int2ptr 来源）反查：地址落在哪个活分配内。
    /// `size < 0` 时按 Miri 语义查 addr-1（消除边界歧义）。
    pub fn lookup(&self, addr: u64, size: i64, alloc_size: impl Fn(AllocId) -> u64) -> Option<AllocId> {
        let addr = if size >= 0 { addr } else { addr.saturating_sub(1) };
        match self.by_addr.binary_search_by_key(&addr, |&(a, _)| a) {
            Ok(pos) => Some(self.by_addr[pos].1),
            Err(0) => None,
            Err(pos) => {
                let (glb, id) = self.by_addr[pos - 1];
                let offset = addr - glb;
                if offset < alloc_size(id) { Some(id) } else { None }
            }
        }
    }

    /// 释放分配：从反查表移除（宿主可能复用该地址给新分配）。
    pub fn on_dealloc(&mut self, id: AllocId) {
        if let Some(&addr) = self.base.get(&id)
            && let Ok(pos) = self.by_addr.binary_search_by_key(&addr, |&(a, _)| a)
            && self.by_addr[pos].1 == id
        {
            self.by_addr.remove(pos);
        }
    }
}
