//! Engine-close tests under a deferred or masked signal, including the close
//! drain and cross-engine finalization handoff.
//!
//! The siblings below hold one subject each; shared fixtures live here.

mod drain;
mod handoff;
mod masked;

use super::*;

const SIGNAL_CLOSE_SOURCE_GUEST_ADDR: u64 = 0xe237;

fn marker_increment(marker: &'static AtomicU64) -> Stmt {
    Stmt::AtomicRmw {
        op: RmwOp::Add,
        addr: Operand::Imm {
            bits: marker.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        dst: ScalarPlace::Slot(word(0)),
        order: MemOrd::SeqCst,
    }
}
