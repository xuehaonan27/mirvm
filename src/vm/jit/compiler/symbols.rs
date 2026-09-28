//! What a finished compile publishes: the `.eh_frame` records held until the whole function is
//! ready, and the symbol ranges the perf map names the result by.

use super::*;

impl<'a> Compiler<'a> {
    /// FrameTable -> eh_frame bytes -> one whole-section registration. The registration
    /// entry keeps the bytes alive for the process lifetime, because the unwinder reads
    /// the shared CIE and each function's FDE out of them later.
    pub(super) fn register_pending_eh_frames(&mut self) {
        if self.pending_unwind.is_empty() {
            return;
        }
        let frames = self
            .pending_unwind
            .drain(..)
            .map(|(id, ui, lsda)| (self.module.get_finalized_function(id) as u64, ui, lsda))
            .collect();
        self.register_eh_frames(frames);
    }

    /// Register a batch of CFA programs at the addresses the code ended up at: the addresses the
    /// module finalized for a fresh compile, or the addresses the link placed them at for a stored
    /// entry.
    pub(super) fn register_eh_frames(&self, frames: Vec<(u64, UnwindInfo, Option<Vec<u8>>)>) {
        super::super::unwind::register_frames(self.module.isa(), frames);
    }

    pub(super) fn finalized_symbol_ranges(
        &self,
        symbols: Vec<PendingJitSymbol>,
    ) -> Vec<JitSymbolRange> {
        symbols
            .into_iter()
            .map(|pending| {
                let guest_name = &self.shared.module.funcs[pending.func as usize].name;
                let start = self.module.get_finalized_function(pending.id) as u64;
                JitSymbolRange::new(
                    self.shared.id,
                    pending.func,
                    pending.role,
                    start,
                    pending.size,
                    guest_name,
                )
            })
            .collect()
    }
}
