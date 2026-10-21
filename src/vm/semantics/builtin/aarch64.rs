//! Maps the frozen NEON builtin shapes onto the architecture's pointer-based ABI.

use crate::vm::ir::Builtin;
use crate::vm::unwind::engine_abort;

pub(super) fn exec_indirect_vector(builtin: &Builtin, av: &[u64], ret_dst: Option<u64>) -> u64 {
    let Some(dst) = ret_dst else {
        engine_abort("NEON return form is not an indirect vector");
    };
    let dst = dst as *mut u8;
    let a = av[0] as *const u8;
    unsafe {
        match *builtin {
            Builtin::NeonAddLong {
                signed,
                lane_bytes,
                vector_bytes,
            } => crate::arch::intrinsics::neon_add_long(dst, a, signed, lane_bytes, vector_bytes),
            Builtin::NeonAddPair {
                lane_bytes,
                vector_bytes,
            } => crate::arch::intrinsics::neon_add_pair(
                dst,
                a,
                av[1] as *const u8,
                lane_bytes,
                vector_bytes,
            ),
            _ => unreachable!(),
        }
    }
    0
}
