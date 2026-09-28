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

    /// Register a batch of CFA programs at the addresses the code ended up at.
    ///
    /// The same path serves a fresh compile (the addresses the module finalized) and a stored entry
    /// (the addresses the link placed it at): the FDE is synthesized here from the stored program, so
    /// no absolute address survives in the stored form.
    pub(super) fn register_eh_frames(&self, frames: Vec<(u64, UnwindInfo, Option<Vec<u8>>)>) {
        if frames.is_empty() {
            return;
        }
        use gimli::RunTimeEndian;
        use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
        unsafe extern "C" {
            fn rust_eh_personality();
        }
        // Two CIEs: functions without a try_call use the plain CIE; the others use a
        // personality CIE whose personality is rust_eh_personality reached indirectly
        // through a DW.ref (lsda_encoding = absptr; embedding an absptr directly does
        // not work) with fde.lsda attached.
        PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
        let isa = self.module.isa();
        let mut table = FrameTable::default();
        let cie_plain = table.add_cie(isa.create_systemv_cie().expect("systemv cie"));
        let mut cie_pers = isa.create_systemv_cie().expect("systemv cie");
        cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        cie_pers.personality = Some((
            gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
            Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
        ));
        let cie_pers_id = table.add_cie(cie_pers);
        for (addr, ui, lsda) in frames {
            if let UnwindInfo::SystemV(info) = ui {
                match lsda {
                    Some(bytes) => {
                        let lsda_addr = bytes.as_ptr() as u64;
                        std::mem::forget(bytes); // the FDE's lsda pointer must outlive this call
                        let mut fde = info.to_fde(Address::Constant(addr));
                        fde.lsda = Some(Address::Constant(lsda_addr));
                        table.add_fde(cie_pers_id, fde);
                    }
                    None => {
                        table.add_fde(cie_plain, info.to_fde(Address::Constant(addr)));
                    }
                }
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        crate::os::unwind::register_frame_section(eh.0.into_vec());
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
