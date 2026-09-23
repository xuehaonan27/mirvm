//! Tree-walking interpreter for a typed Module: the statement/rvalue arms, place and operand
//! evaluation against real addresses, the interpreted frame with its host-recursion stack
//! guard, and the interpreter's half of calling convention v2 (scalar in 1 slot, pair in 2,
//! large aggregates via indirect + sret).
//!
//! Entering a guest function -- deciding between compiled code and this interpreter -- is not
//! here: that is [`crate::vm::dispatch`], which calls back into [`interp_frame`]. What an
//! operation computes is [`crate::vm::semantics`].
//!
//! Unwind: a guest panic is wrapped in a MIRVM-owned exception that keeps the guest standard
//! library's original exception pointer and the owning Engine. An interpreted frame takes
//! the actually escaping exception pointer from a raw catch; `unwind_edge` is set before each
//! unwindable terminator, and the landing boundary decides from that exception's identity
//! whether to run cleanup before propagating on. `FrameGuard` only restores the operand
//! region and the shadow frame. A catch point consumes only its own Engine's guest panics;
//! foreign-owned panics, EngineFaults and host exceptions propagate unchanged.

use std::cell::Cell;

use super::ctx::{Ctx, Engine, Shared};
use super::dispatch::call_guest;
use super::frame::ByteRegion;
use super::instance::Instance;
use super::ir::{
    AsmIoDst, AsmIoVal, Bb, Block, FfiKind, FuncBody, IntBinOp, IntCc, Module, Operand, ParamAbi,
    PlaceBase, PlaceExpr, PlaceStep, RetAbi, RetDest, Rvalue, ScalarPlace, Slot, Stmt, SwitchDiscr,
    Terminator, Width,
};
use super::unwind::engine_abort;

#[derive(Debug)]
pub struct RunError {
    pub kind: RunErrorKind,
    pub message: String,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunErrorKind {
    MissingEntry,
    MissingExport,
    EngineFault,
    EngineClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome<T> {
    Returned(T),
    GuestPanic,
}

/// Untyped two-register return used by the trusted raw export surface.
///
/// Scalar exports use `lo`; pair returns use both words. An indirect return
/// still writes through the caller-provided sret pointer in `args[0]`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawReturn {
    pub lo: u64,
    pub hi: u64,
}

impl<T> RunOutcome<T> {
    pub fn into_returned(self) -> Option<T> {
        match self {
            Self::Returned(value) => Some(value),
            Self::GuestPanic => None,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

mod call;
mod place;
mod runblocks;
mod rvalue;
mod stmt;

pub(crate) use call::interp_frame;
pub(crate) use place::*;

struct FrameGuard {
    ctx: *mut Ctx,
    depth_active: bool,
    base: Option<usize>,
    shadow_active: bool,
    /// Dynamic LSDA: the cleanup edge of the currently unwindable terminator (set before a
    /// Call, cleared once it returns).
    unwind_edge: Cell<Option<Bb>>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        unsafe {
            if self.shadow_active {
                (*self.ctx).shadow.pop(); // pop the shadow frame; it shares the depth's lifetime
            }
            if let Some(base) = self.base {
                region_restore(self.ctx, base);
            }
            if self.depth_active {
                (*self.ctx).depth -= 1;
            }
        }
    }
}

/// Exit from a block-sequence run.
enum Exit {
    Ret(u64, u64),
    /// Tail of a cleanup chain (Resume): return to guard.drop and let the host unwinder
    /// continue.
    Resume,
}

/// Runs the module's `main` through the `lang_start` entry: resolves the entry, executes it
/// under a raw catch, disposes of an uncaught guest panic, and runs the atexit callbacks
/// before reporting the outcome.
pub fn run_main(engine: &Engine) -> Result<RunOutcome<i32>, RunError> {
    let lease = engine.execution_lease().map_err(|_| RunError {
        kind: RunErrorKind::EngineClosed,
        message: "Engine is closed".into(),
        exit_code: 70,
    })?;
    let shared = lease.shared();
    let Some(entry) = shared.module.entry else {
        return Err(RunError {
            kind: RunErrorKind::MissingEntry,
            message: "no `main` entry (lib crate?)".into(),
            exit_code: 2,
        });
    };
    let activation = super::ctx::activate(shared);
    let ctx_ptr = activation.ctx();
    let main_run = super::ctx::begin_main_run(ctx_ptr);
    super::ctx::set_fork_baseline(shared); // pin the fork guard baseline for a single-threaded guest
    let args = [
        shared.instance.resolve_link_addr(entry.main_addr),
        entry.argc,
        entry.argv_ptr,
        entry.sigpipe as u64,
    ];
    let outcome = match super::unwind::catch_raw(|| call_guest(ctx_ptr, entry.lang_start, &args).0)
    {
        Ok(code) => {
            if main_run.finish() {
                RunOutcome::GuestPanic
            } else {
                RunOutcome::Returned(code as i32)
            }
        }
        Err(exception) => match exception.take_mirvm(shared) {
            Ok(super::unwind::MirvmPayload::Guest(payload)) => {
                super::unwind::dispose_uncaught_guest_panic(ctx_ptr, payload);
                RunOutcome::GuestPanic
            }
            Ok(super::unwind::MirvmPayload::EngineFault(fault)) => {
                let fault = super::ctx::drain_current_thread_signal_deliveries_after_fault(
                    ctx_ptr,
                    fault.finish(),
                );
                return Err(RunError {
                    kind: RunErrorKind::EngineFault,
                    message: fault.message,
                    exit_code: fault.code,
                });
            }
            Ok(super::unwind::MirvmPayload::EngineClosed) => {
                return Err(RunError {
                    kind: RunErrorKind::EngineClosed,
                    message: "Engine closed during execution".into(),
                    exit_code: 70,
                });
            }
            Err(exception) => exception.resume_or_rethrow(),
        },
    };
    let exit_code = match outcome {
        RunOutcome::Returned(code) => code,
        RunOutcome::GuestPanic => 101,
    };
    crate::vm::atexit::run_callbacks(ctx_ptr, exit_code);
    Ok(outcome)
}

/// Dev entry: calls an exported function by name.
///
/// Top-level catch: a guest panic escaping the export is an uncaught panic, reported as a
/// diagnostic with exit code 101 (an approximation of native lang_start semantics). A host
/// panic, i.e. a VM bug, propagates unchanged.
///
/// # Safety
///
/// `args` is an untyped ABI slot array. It must match the export's exact
/// lowered parameters. Every slot interpreted as a pointer/reference must
/// satisfy the guest type's validity, alignment, aliasing and lifetime rules
/// for the entire call. For an indirect return, `args[0]` must be the valid
/// sret destination required by that lowered ABI; the returned `RawReturn`
/// words are meaningful only for scalar/pair return ABIs.
pub unsafe fn run_export(
    engine: &Engine,
    name: &str,
    args: &[u64],
) -> Result<RunOutcome<RawReturn>, RunError> {
    let lease = engine.execution_lease().map_err(|_| RunError {
        kind: RunErrorKind::EngineClosed,
        message: "Engine is closed".into(),
        exit_code: 70,
    })?;
    let shared = lease.shared();
    let Some(&id) = shared.module.exports.get(name) else {
        let mut names: Vec<&str> = shared.module.exports.keys().map(|k| &**k).collect();
        names.sort();
        names.retain(|n| !n.starts_with("_ZN") && !n.starts_with("_R"));
        return Err(RunError {
            kind: RunErrorKind::MissingExport,
            message: format!("export `{name}` doesn't exist, available: {names:?}"),
            exit_code: 2,
        });
    };
    let activation = super::ctx::activate(shared);
    let ctx_ptr = activation.ctx();
    super::ctx::set_fork_baseline(shared); // pin the fork guard baseline for a single-threaded guest
    match super::unwind::catch_raw(|| call_guest(ctx_ptr, id, args)) {
        Ok((lo, hi)) => {
            crate::vm::atexit::run_callbacks(ctx_ptr, 0);
            Ok(RunOutcome::Returned(RawReturn { lo, hi }))
        }
        Err(exception) => match exception.take_mirvm(shared) {
            Ok(super::unwind::MirvmPayload::Guest(payload)) => {
                super::unwind::dispose_uncaught_guest_panic(ctx_ptr, payload);
                Ok(RunOutcome::GuestPanic)
            }
            Ok(super::unwind::MirvmPayload::EngineFault(fault)) => {
                let fault = super::ctx::drain_current_thread_signal_deliveries_after_fault(
                    ctx_ptr,
                    fault.finish(),
                );
                Err(RunError {
                    kind: RunErrorKind::EngineFault,
                    message: fault.message,
                    exit_code: fault.code,
                })
            }
            Ok(super::unwind::MirvmPayload::EngineClosed) => Err(RunError {
                kind: RunErrorKind::EngineClosed,
                message: "Engine closed during execution".into(),
                exit_code: 70,
            }),
            Err(exception) => exception.resume_or_rethrow(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::eval_place_addr;
    use crate::vm::ctx::{Engine, Shared};
    use crate::vm::ir::{
        Block, FuncBody, Module, Operand, PlaceBase, PlaceExpr, PlaceStep, RetAbi, Rvalue,
        ScalarPlace, Slot, Stmt, Terminator, Width,
    };
    use crate::vm::semantics::volatile::{mem_read_volatile, mem_write_volatile};

    #[test]
    fn engine_fault_returns_from_export_instead_of_exiting_the_host() {
        let mut module = Module::default();
        module.funcs.push(FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Trap("broken bytecode".into()),
            }],
            name: "trap_export".into(),
        });
        module.exports.insert("trap_export".into(), 0);
        let engine = Engine::new(Shared::new(module));

        let err = unsafe { super::run_export(&engine, "trap_export", &[]) }.unwrap_err();
        assert_eq!(err.kind, super::RunErrorKind::EngineFault);
        assert_eq!(err.exit_code, 70);
        assert!(err.message.contains("broken bytecode"), "{err}");
    }

    #[test]
    fn raw_export_preserves_both_pair_return_words() {
        let lo = Slot {
            off: 0,
            width: Width::W64,
        };
        let hi = Slot {
            off: 8,
            width: Width::W64,
        };
        let mut module = Module::default();
        module.funcs.push(FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Pair(lo, hi),
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(lo),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0x0123_4567_89ab_cdef,
                            width: Width::W64,
                        }),
                    },
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(hi),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0xfedc_ba98_7654_3210,
                            width: Width::W64,
                        }),
                    },
                ],
                term: Terminator::Return,
            }],
            name: "pair_export".into(),
        });
        module.exports.insert("pair".into(), 0);
        let engine = Engine::new(Shared::new(module));

        let result = unsafe { super::run_export(&engine, "pair", &[]) }
            .unwrap()
            .into_returned()
            .unwrap();
        assert_eq!(result.lo, 0x0123_4567_89ab_cdef);
        assert_eq!(result.hi, 0xfedc_ba98_7654_3210);
    }

    #[test]
    fn dyn_tail_alignment_preserves_prefixes_larger_than_four_gibibytes() {
        let vtable = [0u64, 0, 32];
        let unaligned = u32::MAX as u64 + 18;
        let expr = PlaceExpr {
            base: PlaceBase::Local(0x1000),
            steps: vec![PlaceStep::VTableAlignOffset {
                meta: Operand::Imm {
                    bits: vtable.as_ptr() as u64,
                    width: Width::W64,
                },
                unaligned,
                packed: None,
            }]
            .into_boxed_slice(),
        };
        let expected_offset = (unaligned + 31) & !31;
        assert_eq!(
            eval_place_addr(std::ptr::null_mut(), 0, &expr),
            0x1000 + expected_offset
        );
    }

    #[test]
    fn volatile_scalar_roundtrip_preserves_each_width() {
        for (size, value) in [
            (1, 0xa5),
            (2, 0xb6a5),
            (4, 0xd8c7_b6a5),
            (8, 0xf0e9_d8c7_b6a5_9483),
        ] {
            let mut storage = [0u8; 8];
            let mut got = 0u64;
            mem_write_volatile(
                storage.as_mut_ptr() as u64,
                (&value as *const u64) as u64,
                size,
            );
            mem_read_volatile(storage.as_ptr() as u64, (&mut got as *mut u64) as u64, size);
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            assert_eq!(got, value & mask);
        }
    }

    #[test]
    fn volatile_unaligned_roundtrip_does_not_require_host_alignment() {
        let mut storage = [0u8; 16];
        let addr = unsafe { storage.as_mut_ptr().add(1) } as u64;
        let value = 0xf0e9_d8c7_b6a5_9483;
        let mut got = 0u64;
        mem_write_volatile(addr, (&value as *const u64) as u64, 8);
        mem_read_volatile(addr, (&mut got as *mut u64) as u64, 8);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_16_byte_roundtrip_preserves_the_whole_value() {
        let mut storage = [0u8; 16];
        let value = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let mut got = [0u8; 16];
        mem_write_volatile(storage.as_mut_ptr() as u64, value.as_ptr() as u64, 16);
        mem_read_volatile(storage.as_ptr() as u64, got.as_mut_ptr() as u64, 16);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_16_byte_value_may_have_alignment_one() {
        let mut storage = [0u8; 24];
        let base = storage.as_mut_ptr() as usize;
        let offset = (9 - base % 8) % 8;
        let addr = unsafe { storage.as_mut_ptr().add(offset) } as u64;
        assert_eq!(addr % 8, 1);
        let value = [0xa5u8; 16];
        let mut got = [0u8; 16];
        mem_write_volatile(addr, value.as_ptr() as u64, 16);
        mem_read_volatile(addr, got.as_mut_ptr() as u64, 16);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_wide_store_preserves_every_byte() {
        let source: Vec<u8> = (0..137)
            .map(|index| (index as u8).wrapping_mul(17))
            .collect();
        let mut storage = vec![0u8; source.len() + 1];
        mem_write_volatile(
            unsafe { storage.as_mut_ptr().add(1) } as u64,
            source.as_ptr() as u64,
            source.len() as u32,
        );
        assert_eq!(&storage[1..], source.as_slice());
    }

    #[test]
    fn volatile_unaligned_31_byte_roundtrip_covers_every_chunk_width() {
        let source: Vec<u8> = (0..31)
            .map(|index| (index as u8).wrapping_mul(29))
            .collect();
        let mut storage = [0u8; 33];
        let mut got = [0u8; 31];
        let unaligned = unsafe { storage.as_mut_ptr().add(1) };
        mem_write_volatile(unaligned as u64, source.as_ptr() as u64, 31);
        mem_read_volatile(unaligned as u64, got.as_mut_ptr() as u64, 31);
        assert_eq!(got.as_slice(), source.as_slice());
    }

    #[test]
    fn volatile_wide_load_snapshots_before_overlapping_destination() {
        let mut storage: Vec<u8> = (0..160).map(|index| index as u8).collect();
        let expected = storage[..137].to_vec();
        let base = storage.as_mut_ptr();
        mem_read_volatile(base as u64, unsafe { base.add(7) } as u64, 137);
        assert_eq!(&storage[7..144], expected.as_slice());
    }

    #[test]
    fn volatile_wide_store_snapshots_before_overlapping_destination() {
        let mut storage: Vec<u8> = (0..160).map(|index| (index as u8) ^ 0xa5).collect();
        let expected = storage[..137].to_vec();
        let base = storage.as_mut_ptr();
        mem_write_volatile(unsafe { base.add(7) } as u64, base as u64, 137);
        assert_eq!(&storage[7..144], expected.as_slice());
    }

    #[test]
    fn volatile_padded_aggregate_never_interprets_padding_as_an_integer() {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Padded {
            tag: u8,
            value: u32,
        }

        let value = Padded {
            tag: 0xa5,
            value: 0x1234_5678,
        };
        let mut storage = MaybeUninit::<Padded>::uninit();
        let mut got = MaybeUninit::<Padded>::uninit();
        mem_write_volatile(
            storage.as_mut_ptr() as u64,
            (&value as *const Padded) as u64,
            size_of::<Padded>() as u32,
        );
        mem_read_volatile(
            storage.as_ptr() as u64,
            got.as_mut_ptr() as u64,
            size_of::<Padded>() as u32,
        );
        let got = unsafe { got.assume_init() };
        assert_eq!(got.tag, value.tag);
        assert_eq!(got.value, value.value);
    }
}
