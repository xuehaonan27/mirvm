//! Engine lifecycle and fault tests: guest panics, engine faults, close
//! ordering and the teardown boundary.
//!
//! The siblings below hold one subject each; shared fixtures live here.

mod close;
mod fault;
mod panic;
mod teardown;

use super::*;

#[derive(Debug, Eq, PartialEq)]
struct HostPanicMarker(u64);

fn guest_panic_function(name: &str, params: usize) -> FuncBody {
    function(
        name,
        params,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::UnwindRaise,
            args: vec![Operand::Imm {
                bits: 0x4747,
                width: Width::W64,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: crate::vm::ir::BuiltinCallRole::Normal,
        },
    )
}
