//! 地址表：给每个 AllocId 惰性分配唯一的"物理"地址（Miri intptrcast 的简化版）。
//!
//! Provenance::OFFSET_IS_ADDR = true 的世界里，Pointer.offset 存绝对地址；
//! 这张表维护 AllocId ↔ 基址 双向映射，支撑 ptr↔int cast 与 wildcard 指针解引用。
//! 简化：单调 bump 分配、不复用地址、全部分配视为已 expose。

use rustc_abi::{Align, Size};
use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::AllocId;

#[derive(Debug)]
pub struct AddrTable {
    next: u64,
    base: FxHashMap<AllocId, u64>,
    /// 按基址排序的 (base, id)；死分配会被移除（base 映射保留，用于报错时算偏移）。
    by_addr: Vec<(u64, AllocId)>,
}

impl Default for AddrTable {
    fn default() -> Self {
        // 避开低地址（null、ZST dangling 常用小整数地址）。
        AddrTable { next: 1 << 32, base: FxHashMap::default(), by_addr: Vec::new() }
    }
}

impl AddrTable {
    pub fn base_of(&self, id: AllocId) -> Option<u64> {
        self.base.get(&id).copied()
    }

    /// 返回（或分配）`id` 的基址。
    pub fn addr_for(&mut self, id: AllocId, size: Size, align: Align) -> u64 {
        if let Some(&addr) = self.base.get(&id) {
            return addr;
        }
        let align = align.bytes().max(1);
        let base = self.next.next_multiple_of(align);
        // ZST 也占 1 字节地址空间，保证函数指针等地址唯一；外加少量隔离带。
        self.next = base + size.bytes().max(1) + 16;
        self.base.insert(id, base);
        debug_assert!(self.by_addr.last().is_none_or(|&(a, _)| a < base));
        self.by_addr.push((base, id));
        base
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

    /// 释放分配：从反查表移除（基址映射保留）。
    pub fn on_dealloc(&mut self, id: AllocId) {
        if let Some(&addr) = self.base.get(&id)
            && let Ok(pos) = self.by_addr.binary_search_by_key(&addr, |&(a, _)| a)
        {
            self.by_addr.remove(pos);
        }
    }
}
