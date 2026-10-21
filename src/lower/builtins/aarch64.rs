//! AArch64 LLVM intrinsic names and their frozen integer/vector shapes.

use rustc_data_structures::fx::FxHashMap;
use rustc_span::Symbol;

use crate::vm::ir;

/// Register every guest intrinsic name this CPU's lowering can produce.
pub(super) fn register(out: &mut FxHashMap<Symbol, ir::Builtin>) {
    for vector_bytes in [8, 16] {
        for lane_bytes in [1, 2, 4] {
            let lanes = vector_bytes / lane_bytes;
            let bits = lane_bytes * 8;
            for (name, signed) in [("uaddlp", false), ("saddlp", true)] {
                out.insert(
                    Symbol::intern(&format!(
                        "llvm.aarch64.neon.{name}.v{}i{}.v{lanes}i{bits}",
                        lanes / 2,
                        bits * 2
                    )),
                    ir::Builtin::NeonAddLong {
                        signed,
                        lane_bytes,
                        vector_bytes,
                    },
                );
            }
        }
        for lane_bytes in [1, 2, 4, 8] {
            let lanes = vector_bytes / lane_bytes;
            if lanes < 2 {
                continue;
            }
            out.insert(
                Symbol::intern(&format!(
                    "llvm.aarch64.neon.addp.v{lanes}i{}",
                    lane_bytes * 8
                )),
                ir::Builtin::NeonAddPair {
                    lane_bytes,
                    vector_bytes,
                },
            );
        }
    }
    for (prefix, castagnoli) in [("crc32", false), ("crc32c", true)] {
        for (suffix, bytes) in [("b", 1), ("h", 2), ("w", 4), ("x", 8)] {
            out.insert(
                Symbol::intern(&format!("llvm.aarch64.{prefix}{suffix}")),
                ir::Builtin::Aarch64Crc32 { castagnoli, bytes },
            );
        }
    }
}
