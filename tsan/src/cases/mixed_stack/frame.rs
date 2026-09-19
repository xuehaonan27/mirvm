//! Slaved operand region: model A's frame-local storage (spike 1 prototype).
//!
//! Every frame's local slots are cut from one contiguous `Vec`; a frame `reserve`s a run on
//! entry and `restore`s it on return. The interface stays narrow and is deliberately *not*
//! coupled to the fast/checked safety mode: spike 1 is fast and performs no range checks.

pub type Word = u64;

/// A contiguous slot stack. Frames use it LIFO through reserve/restore.
#[derive(Default)]
pub struct OperandRegion {
    slots: Vec<Word>,
}

impl OperandRegion {
    pub fn new() -> Self {
        OperandRegion { slots: Vec::new() }
    }

    /// Cut `n` zeroed slots for a new frame; returns the base (the region's end before the
    /// cut).
    pub fn reserve(&mut self, n: u32) -> usize {
        let base = self.slots.len();
        self.slots.resize(base + n as usize, 0);
        base
    }

    /// Pop this frame's slots (truncate back to `base`). Strictly paired with `reserve`.
    pub fn restore(&mut self, base: usize) {
        self.slots.truncate(base);
    }

    #[inline]
    pub fn read(&self, base: usize, slot: u32) -> Word {
        self.slots[base + slot as usize]
    }

    #[inline]
    pub fn write(&mut self, base: usize, slot: u32, v: Word) {
        self.slots[base + slot as usize] = v;
    }
}
