//! Unwind registration at the addresses code actually landed at.
//!
//! A fresh compile and a linked store entry register the same thing: one FDE per symbol, synthesized
//! here from the CFA program the backend produced, so no absolute address survives in a stored entry.
//! Two CIEs: the plain one, and the one whose personality is `rust_eh_personality` reached through a
//! DW.ref, which every frame carrying an LSDA uses.
//!
//! Two things outlive this call because the unwinder keeps their addresses: the section it registers,
//! and the LSDA bytes an FDE points at. Both are leaked here rather than borrowed, which is the same
//! ownership a caller that hands over a section has on the platform whose entry point takes one.

use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use std::sync::atomic::{AtomicU64, Ordering};

/// DW.ref indirection cell for the personality CIE: one cell shared by every FDE in the table.
static PERS_REF: AtomicU64 = AtomicU64::new(0);

use gimli::RunTimeEndian;
use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
unsafe extern "C" {
    fn rust_eh_personality();
}

/// One frame to register: the address the code landed at, its CFA program, and the landing-pad table
/// that program's FDE points at when the frame carries one.
pub(crate) type Frame = (u64, UnwindInfo, Option<Vec<u8>>);

/// Whether a frame's CFA program becomes an FDE on this target.
///
/// Cranelift picks the kind from the target OS — SystemV everywhere but Windows — so on the pairs
/// mirvm builds this is always true. The check is not decoration: publishing a frame whose CFI this
/// engine cannot express leaves the unwinder nothing to walk, and a walk that passes over a frame is
/// a wrong unwind, not a missing one.
pub(crate) fn is_registrable(info: &UnwindInfo) -> bool {
    matches!(info, UnwindInfo::SystemV(_))
}

/// Register a batch of CFA programs at the addresses the code ended up at.
///
/// The same path serves a fresh compile (the addresses the module finalized) and a stored entry (the
/// addresses the link placed it at): the FDE is synthesized here from the stored program, so no
/// absolute address survives in the stored form.
///
/// Every frame must be registrable ([`is_registrable`]): a caller screens its frames *before* it
/// publishes anything, so a frame this engine cannot describe never reaches a running session.
pub(crate) fn register_frames(isa: &dyn TargetIsa, frames: Vec<Frame>) {
    if frames.is_empty() {
        return;
    }
    // Two CIEs: functions without a try_call use the plain CIE; the others use a
    // personality CIE whose personality is rust_eh_personality reached indirectly
    // through a DW.ref (lsda_encoding = absptr; embedding an absptr directly does
    // not work) with fde.lsda attached.
    PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
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
        let UnwindInfo::SystemV(info) = ui else {
            // A caller screens its frames before publishing them, so this is a bug in this process
            // rather than a kind of input: it trips a debug build and stays a documented hole in a
            // release one instead of panicking on the compile worker.
            debug_assert!(false, "a frame with no FDE reached registration: {ui:?}");
            continue;
        };
        match lsda {
            Some(bytes) => {
                // The FDE carries the address of these bytes and the unwinder reads them on every
                // walk through this frame, so they are leaked on purpose: they belong to the process
                // now, like the section below. `frames` is this call's own, so a borrow would leave
                // the FDE pointing at freed memory the moment this function returned.
                let lsda_addr = bytes.leak().as_ptr() as u64;
                let mut fde = info.to_fde(Address::Constant(addr));
                fde.lsda = Some(Address::Constant(lsda_addr));
                table.add_fde(cie_pers_id, fde);
            }
            None => {
                table.add_fde(cie_plain, info.to_fde(Address::Constant(addr)));
            }
        }
    }
    let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
    table.write_eh_frame(&mut eh).unwrap();
    crate::os::unwind::register_frame_section(eh.0.into_vec());
}
