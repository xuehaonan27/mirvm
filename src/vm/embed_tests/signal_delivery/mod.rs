//! Signal delivery tests: masks, old-action reinstatement, target-thread
//! delivery, sigwaitinfo and `wait_closed` while a signal is pending.
//!
//! The siblings below hold one subject each; shared fixtures live here.

mod mask;
mod oldact;
mod sigwaitinfo;
mod target;
mod wait_closed;

use super::*;

static SIGNAL_TARGET_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);

static SIGNAL_TARGET_HANDLER_THREAD: AtomicU64 = AtomicU64::new(0);

static SIGNAL_TARGET_WORKER_THREAD: AtomicU64 = AtomicU64::new(0);

const SIGNAL_TARGET_GUEST_ADDR: u64 = 0xe23a;

unsafe extern "C-unwind" fn signal_target_blocking_entry() {
    SIGNAL_TARGET_WORKER_THREAD.store(
        crate::os::thread::current_thread().as_u64(),
        Ordering::SeqCst,
    );
    unsafe { lifecycle_blocking_entry() };
}

unsafe extern "C-unwind" fn record_signal_target_handler_thread() {
    SIGNAL_TARGET_HANDLER_THREAD.store(
        crate::os::thread::current_thread().as_u64(),
        Ordering::SeqCst,
    );
    SIGNAL_TARGET_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

fn target_thread_signal_module() -> Module {
    let handler = function(
        "record_target_thread_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: record_signal_target_handler_thread as *const () as usize as u64,
                width: Width::W64,
            },
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            null_ok: false,
            native_sig: Some(callback_sig()),
        },
    );
    let safe = function(
        "target_thread_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let raise = function(
        "target_thread_wrapped_raise",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let block = calls_native(
        "target_thread_blocks_inside_native_call",
        signal_target_blocking_entry,
    );
    let mut module = Module {
        funcs: vec![handler, safe, raise, block].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.exports.insert("raise".into(), 2);
    module.exports.insert("block".into(), 3);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_TARGET_GUEST_ADDR), 0));
    module
}
