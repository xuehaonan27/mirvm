//! Embedding tests for the engine, split by subject into the sibling modules
//! below. Shared statics, fixtures and module builders live here; the siblings
//! reach them through `use super::*` and `super::<submodule>`.

mod capture;
mod engine_lifecycle;
mod signal_close;
mod signal_delivery;
mod signal_lifecycle;

use std::cell::RefCell;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};

use super::ctx::{self, Engine, Shared};
use super::interp::{RunErrorKind, RunOutcome, run_export, run_main};
use super::ir::{
    self, Block, Builtin, EntryPlan, FfiKind, ForeignSig, FuncBody, GuestPanicCleanup, LinkAddr,
    MemOrd, Module, Operand, ParamAbi, RetAbi, RetDest, RmwOp, Rvalue, ScalarPlace, Slot, Stmt,
    SwitchDiscr, Terminator, UnwindAction, Width,
};
use super::signal;
use super::thunks;
use super::unwind;
use crate::os::signal::{MaskOp, SIGUSR1, SignalMask, set_thread_mask};

static SIGNAL_OWNER_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_NATIVE_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_FIRST_EXTERNAL_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_WAIT_RESULT: AtomicU64 = AtomicU64::new(0);
static NEXT_NATIVE_FIXTURE: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_GATE: LazyLock<(Mutex<(bool, bool)>, Condvar)> =
    LazyLock::new(|| (Mutex::new((false, false)), Condvar::new()));
static SIGNAL_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const SIGNAL_OWNER_GUEST_ADDR: u64 = 0xe231;
const SIGNAL_OVERRIDE_GUEST_ADDR: u64 = 0xe232;

thread_local! {
    static NESTED_ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

unsafe extern "C-unwind" fn lifecycle_blocking_entry() {
    let (lock, changed) = &*LIFECYCLE_GATE;
    let mut state = lock.lock().unwrap();
    state.0 = true;
    changed.notify_all();
    while !state.1 {
        state = changed.wait(state).unwrap();
    }
}

fn wait_for_owner_signal_pending(engine: &Engine) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !super::signal::has_engine_pending(engine.control()) {
        assert!(
            std::time::Instant::now() < deadline,
            "kernel signal frame did not publish to its owner inbox"
        );
        std::thread::yield_now();
    }
}

fn wait_for_signal_marker(marker: &AtomicU64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while marker.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "signal handler did not update its observable marker"
        );
        std::thread::yield_now();
    }
}

unsafe extern "C" fn preserved_native_signal(_signum: i32) {
    SIGNAL_NATIVE_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn first_external_native_signal(_signum: i32) {
    SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C-unwind" fn read_current_errno() -> u64 {
    unsafe { *crate::os::process::errno_location() as u64 }
}

unsafe extern "C-unwind" fn unblock_usr1_inside_signal_handler() {
    let mask = SignalMask::empty().with(SIGUSR1);
    assert!(set_thread_mask(MaskOp::Unblock, &mask).is_ok());
}

struct SavedSignalDisposition {
    signum: i32,
    action: crate::os::signal::Sigaction,
}

impl SavedSignalDisposition {
    fn replace_with_native(signum: i32) -> (Self, crate::os::signal::Sigaction) {
        let action = crate::os::signal::Sigaction::query(signum)
            .expect("failed to save host signal disposition");
        let native =
            crate::os::signal::Sigaction::for_signal(preserved_native_signal as *const () as usize);
        assert_eq!(native.install(signum), 0, "failed to isolate signal test");
        let installed = crate::os::signal::Sigaction::query(signum)
            .expect("failed to query isolated host signal disposition");
        (Self { signum, action }, installed)
    }
}

impl Drop for SavedSignalDisposition {
    fn drop(&mut self) {
        let _ = self.action.install(self.signum);
    }
}

unsafe extern "C-unwind" fn lifecycle_wait_from_callback() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("lifecycle wait Engine was not installed")
            .clone()
    });
    let observed = match engine.wait_closed() {
        Err(super::ctx::WaitClosedError::ActiveOnCurrentThread) => 1,
        _ => 2,
    };
    LIFECYCLE_WAIT_RESULT.store(observed, Ordering::SeqCst);
}

fn reset_lifecycle_gate() {
    *LIFECYCLE_GATE.0.lock().unwrap() = (false, false);
}

fn wait_lifecycle_entry() {
    let (lock, changed) = &*LIFECYCLE_GATE;
    let mut state = lock.lock().unwrap();
    while !state.0 {
        state = changed.wait(state).unwrap();
    }
}

fn release_lifecycle_entry() {
    let (lock, changed) = &*LIFECYCLE_GATE;
    lock.lock().unwrap().1 = true;
    changed.notify_all();
}

fn word(off: u32) -> Slot {
    Slot {
        off,
        width: Width::W64,
    }
}

fn function(name: &str, params: usize, ret: RetAbi, first: Terminator) -> FuncBody {
    FuncBody {
        frame_size: 8 * (params as u32 + 1),
        frame_align: 8,
        ret,
        params: (0..params)
            .map(|index| ParamAbi::Scalar(word(8 * (index as u32 + 1))))
            .collect(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: first,
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: name.into(),
    }
}

fn install_fake_guest_panic_cleanup(module: &mut Module) {
    let cleanup = module.funcs.len() as u32;
    module.funcs.push(FuncBody {
        frame_size: 24,
        frame_align: 8,
        ret: RetAbi::Pair(word(0), word(8)),
        params: vec![ParamAbi::Scalar(word(16))],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: Vec::new(),
            term: Terminator::Return,
        }],
        name: "fake_guest_panic_cleanup".into(),
    });
    let drop_payload = module.funcs.len() as u32;
    module.funcs.push(function(
        "fake_guest_panic_drop_payload",
        1,
        RetAbi::Zst,
        Terminator::Return,
    ));
    module.guest_panic_cleanup = Some(GuestPanicCleanup {
        cleanup,
        drop_payload,
    });
}

fn engine_fault_function(name: &str, params: usize) -> FuncBody {
    function(
        name,
        params,
        RetAbi::Zst,
        Terminator::Trap("embedding contract engine fault".into()),
    )
}

fn export_module(body: FuncBody) -> Module {
    let mut module = Module {
        funcs: vec![body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    install_fake_guest_panic_cleanup(&mut module);
    module
}

fn callback_sig() -> ForeignSig {
    ForeignSig {
        args: Vec::new(),
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: true,
    }
}

fn signal_callback_sig() -> ForeignSig {
    ForeignSig {
        args: vec![FfiKind::I32],
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: false,
    }
}

fn engine(module: Module, jit: bool) -> Engine {
    let mut shared = Shared::new(module);
    shared.jit.enabled = jit;
    if jit {
        shared.jit.threshold = 1;
        shared.jit.sync = true;
    }
    Engine::new(shared)
}

struct NativeFixtureDir(PathBuf);

impl NativeFixtureDir {
    fn new(name: &str) -> Self {
        let serial = NEXT_NATIVE_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("mirvm-{name}-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for NativeFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn native_signal_handler_library() -> (NativeFixtureDir, PathBuf) {
    let directory = NativeFixtureDir::new("native-signal-handler");
    let source = directory.path().join("signal_handler.c");
    let object = directory.path().join("signal_handler.o");
    let archive = directory.path().join("libsignal_handler.a");
    std::fs::write(
        &source,
        r#"
#include <signal.h>
#include <stdint.h>
#include <errno.h>

static volatile uint64_t trace;
static volatile sig_atomic_t finalizer_signal;

__attribute__((destructor)) static void image_finalizer_raise(void) {
    int signum = finalizer_signal;
    if (signum != 0) {
        trace = trace * 10 + 7;
        trace = trace * 10 + (raise(signum) == 0 ? 8 : 9);
    }
}

void arm_image_finalizer_raise(int signum) {
    trace = 0;
    finalizer_signal = signum;
}

static void image_handler(int signum) {
    sigset_t current;
    (void)signum;
    if (pthread_sigmask(SIG_SETMASK, 0, &current) != 0 ||
        sigismember(&current, SIGUSR1) != 1 ||
        sigismember(&current, SIGTERM) != 1 ||
        sigismember(&current, SIGHUP) != 1) {
        trace = trace * 10 + 9;
    } else {
        trace = trace * 10 + 1;
    }
    raise(SIGUSR2);
    trace = trace * 10 + 3;
}

static void image_fault_handler(int signum) {
    (void)signum;
    trace = trace * 10 + 4;
    raise(SIGUSR2);
    trace = trace * 10 + 6;
}

void install_image_handlers(void (*guest_handler)(int)) {
    struct sigaction action = {0};
    trace = 0;
    signal(SIGUSR2, guest_handler);
    action.sa_handler = image_handler;
    action.sa_flags = SA_RESTART;
    sigemptyset(&action.sa_mask);
    sigaddset(&action.sa_mask, SIGTERM);
    sigaction(SIGUSR1, &action, 0);
}

void install_image_fault_handlers(void (*guest_handler)(int)) {
    trace = 0;
    signal(SIGUSR2, guest_handler);
    signal(SIGUSR1, image_fault_handler);
}

void record_nested_guest(int signum) {
    (void)signum;
    trace = trace * 10 + 2;
}

static uint64_t packed_signal_result(int failed) {
    return ((uint64_t)(uint32_t)errno << 32) | (uint32_t)failed;
}

uint64_t image_signal_invalid_signum(void) {
    errno = 0;
    return packed_signal_result(signal(0, SIG_DFL) == SIG_ERR);
}

uint64_t image_sigaction_invalid_signum(void) {
    errno = 0;
    return packed_signal_result(sigaction(0, 0, 0) == -1);
}

uint64_t image_signal_sigkill(void (*guest_handler)(int)) {
    errno = 0;
    return packed_signal_result(signal(SIGKILL, guest_handler) == SIG_ERR);
}

uint64_t image_sigaction_sigkill(void (*guest_handler)(int)) {
    struct sigaction action = {0};
    action.sa_handler = guest_handler;
    sigemptyset(&action.sa_mask);
    errno = 0;
    return packed_signal_result(sigaction(SIGKILL, &action, 0) == -1);
}

void image_signal_realtime(void (*guest_handler)(int)) {
    (void)signal(SIGRTMIN, guest_handler);
}

void image_signal_sync_fault(void (*guest_handler)(int)) {
    (void)signal(SIGSEGV, guest_handler);
}

void image_sigaction_siginfo(void (*guest_handler)(int)) {
    struct sigaction action = {0};
    action.sa_handler = guest_handler;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    (void)sigaction(SIGUSR1, &action, 0);
}

void image_signal_closed_handler(void (*handler)(int)) {
    (void)signal(SIGUSR1, handler);
}

uint64_t read_image_signal_trace(void) { return trace; }
"#,
    )
    .unwrap();
    let cc = Command::new("cc")
        .args(["-fPIC", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .output()
        .unwrap();
    assert!(
        cc.status.success(),
        "failed to compile native signal handler fixture:\n{}{}",
        String::from_utf8_lossy(&cc.stdout),
        String::from_utf8_lossy(&cc.stderr)
    );
    let ar = Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .output()
        .unwrap();
    assert!(
        ar.status.success(),
        "failed to archive native signal handler fixture:\n{}{}",
        String::from_utf8_lossy(&ar.stdout),
        String::from_utf8_lossy(&ar.stderr)
    );
    let library =
        crate::native::archive::materialize_in(&archive, &directory.path().join("materialized"))
            .unwrap();
    (directory, library)
}

fn modes() -> Vec<(&'static str, bool)> {
    #[allow(unused_mut)]
    let mut modes = vec![("interp", false)];
    #[cfg(feature = "cranelift")]
    modes.push(("jit", true));
    modes
}

fn calls_native(name: &str, entry: unsafe extern "C-unwind" fn()) -> FuncBody {
    function(
        name,
        0,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: entry as usize as u64,
                width: Width::W64,
            },
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            null_ok: false,
            native_sig: Some(callback_sig()),
        },
    )
}

fn marker_store(marker: &'static AtomicU64) -> Stmt {
    Stmt::AtomicStore {
        addr: Operand::Imm {
            bits: marker.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        order: MemOrd::SeqCst,
    }
}

fn signal_owner_module(handler_addr: u64, marker: &'static AtomicU64) -> Module {
    let mut handler = function(
        "owned_process_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0].stmts.push(marker_store(marker));
    let mut module = Module {
        funcs: vec![
            handler,
            function(
                "owner_signal_safe_point",
                0,
                RetAbi::Zst,
                Terminator::Return,
            ),
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.fn_entry_links.push((LinkAddr(handler_addr), 0));
    module
}

fn physically_masked_signal_module() -> Module {
    let mut module = signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN);
    let raise = function(
        "raise_under_preexisting_host_mask",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let id = module.funcs.len() as u32;
    module.funcs.push(raise);
    module.exports.insert("raise".into(), id);
    module
}

fn first_external_native_signal_module() -> Module {
    let old = word(0);
    let install = function(
        "install_first_seen_external_native_signal_handler",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: first_external_native_signal as *const () as usize as u64,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![install].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 0);
    module
}

fn signal_p1_owner_module(link_addr: LinkAddr, marker: &'static AtomicU64) -> Module {
    let mut module = signal_owner_module(link_addr.0, marker);
    let old = word(0);
    let installer = function(
        "install_owned_p1_signal_handler",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::AddrImm(link_addr),
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    module.funcs.push(installer);
    module.exports.insert("install".into(), 2);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

/// An action a test drives the kernel ABI with: a handler, the given mask, and no flags, which
/// the test then adds through the accessors.
fn raw_sigaction(handler: usize, mask: &[i32]) -> crate::os::signal::Sigaction {
    let mut action = crate::os::signal::Sigaction::empty(handler, 0);
    for &signum in mask {
        action.add_to_mask(signum);
    }
    action
}

struct NestedEngineGuard;

impl Drop for NestedEngineGuard {
    fn drop(&mut self) {
        NESTED_ENGINE.with(|slot| {
            slot.replace(None);
        });
    }
}

fn with_nested_engine<R>(engine: &Engine, f: impl FnOnce() -> R) -> R {
    NESTED_ENGINE.with(|slot| {
        assert!(
            slot.replace(Some(engine.clone())).is_none(),
            "nested Engine test entry was already occupied"
        );
    });
    let _guard = NestedEngineGuard;
    f()
}
