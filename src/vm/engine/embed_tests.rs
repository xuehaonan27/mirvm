use std::cell::RefCell;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};

use super::ctx::{Engine, Shared};
use super::interp::{RunErrorKind, RunOutcome, run_export, run_main};
use super::ir::{
    Block, Builtin, EntryPlan, FfiKind, ForeignSig, FuncBody, GuestPanicCleanup, LinkAddr, MemOrd,
    Module, Operand, ParamAbi, RetAbi, RetDest, RmwOp, Rvalue, ScalarPlace, Slot, Stmt,
    SwitchDiscr, Terminator, UnwindAction, Width,
};
use super::thunks;
use super::unwind;

static FOREIGN_CATCH_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_CATCH_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_CLOSED_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_BOUNDARY_RESULT: AtomicU64 = AtomicU64::new(0);
static SUSPENDED_FAULT_GUEST_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_SIGNAL_NESTED_RAN: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN: AtomicU64 = AtomicU64::new(u64::MAX);
static SIGNAL_OWNER_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_OVERRIDE_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_NATIVE_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_FIRST_EXTERNAL_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_EXTERNAL_SIGINFO_CODE: AtomicI32 = AtomicI32::new(i32::MIN);
static SIGNAL_EXTERNAL_WAIT_ENTERED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_EXTERNAL_WAIT_RELEASED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_HANDLER_THREAD: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_WORKER_THREAD: AtomicU64 = AtomicU64::new(0);
static SIGNAL_MASKED_OLD_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_MASKED_NEW_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_MASKED_CLOSE_RETURNED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_CLOSE_CHAIN_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_WAIT_RESULT: AtomicU64 = AtomicU64::new(0);
static THUNK_MAIN_CATCH_RESULT: AtomicU64 = AtomicU64::new(0);
static NEXT_NATIVE_FIXTURE: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_GATE: LazyLock<(Mutex<(bool, bool)>, Condvar)> =
    LazyLock::new(|| (Mutex::new((false, false)), Condvar::new()));
static SIGNAL_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const LIFECYCLE_SIGNAL_GUEST_ADDR: u64 = 0xe221;
const SIGNAL_OWNER_GUEST_ADDR: u64 = 0xe231;
const SIGNAL_OVERRIDE_GUEST_ADDR: u64 = 0xe232;
const SIGNAL_MASKED_OLD_GUEST_ADDR: u64 = 0xe233;
const SIGNAL_MASKED_NEW_GUEST_ADDR: u64 = 0xe234;
const SIGNAL_MASKING_CLOSE_GUEST_ADDR: u64 = 0xe235;
const SIGNAL_IMAGE_NESTED_GUEST_ADDR: u64 = 0xe236;
const SIGNAL_CLOSE_SOURCE_GUEST_ADDR: u64 = 0xe237;
const SIGNAL_CLOSE_CHAIN_GUEST_ADDR: u64 = 0xe238;
const SIGNAL_FAULTING_P1_GUEST_ADDR: u64 = 0xe239;
const SIGNAL_TARGET_GUEST_ADDR: u64 = 0xe23a;

thread_local! {
    static NESTED_ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

#[derive(Debug, Eq, PartialEq)]
struct HostPanicMarker(u64);

unsafe extern "C-unwind" fn host_panic_entry() {
    std::panic::panic_any(HostPanicMarker(0xe13));
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

unsafe extern "C-unwind" fn signal_target_blocking_entry() {
    SIGNAL_TARGET_WORKER_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
    unsafe { lifecycle_blocking_entry() };
}

unsafe extern "C-unwind" fn record_signal_target_handler_thread() {
    SIGNAL_TARGET_HANDLER_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
    SIGNAL_TARGET_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C-unwind" fn lifecycle_close_and_record_signal() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("lifecycle reentry Engine was not installed")
            .clone()
    });
    engine.close();
    let _queue = engine.shared().jit.queue.lock().unwrap();
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
    wait_for_owner_signal_pending(&engine);
    LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.store(
        LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst),
        Ordering::SeqCst,
    );
}

unsafe extern "C-unwind" fn process_kill_usr1() {
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("signal owner Engine was not installed")
            .clone()
    });
    wait_for_owner_signal_pending(&engine);
}

unsafe extern "C-unwind" fn close_nested_engine_from_signal() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("masked close Engine was not installed")
            .clone()
    });
    engine.close();
    assert_eq!(
        SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
        0,
        "closing an Engine under another handler's mask finalized it before the handler returned"
    );
    SIGNAL_MASKED_CLOSE_RETURNED.store(1, Ordering::SeqCst);
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

fn physically_masked_close_child_finish(
    engine: Engine,
    mask_guard: crate::os::signal::ThreadSignalMaskGuard,
    baseline: &crate::os::signal::Sigaction,
    wrapped_raise: bool,
) {
    assert_ne!(
        crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
            & (1u64 << libc::SIGUSR1),
        0
    );
    engine.wait_closed().unwrap();
    assert_eq!(
        SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
        u64::from(!wrapped_raise)
    );
    assert_eq!(engine.state(), super::ctx::EngineState::Closed);
    assert_ne!(
        crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
            & (1u64 << libc::SIGUSR1),
        0,
        "Engine close changed the caller's preexisting pthread mask"
    );
    assert!(
        crate::os::signal::Sigaction::query(libc::SIGUSR1)
            .unwrap()
            .same_disposition(baseline)
    );
    drop(mask_guard);
    if wrapped_raise {
        wait_for_signal_marker(&SIGNAL_NATIVE_HANDLER_RAN);
        assert_eq!(SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst), 1);
        assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 0);
    } else {
        assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
    }
}

unsafe extern "C" fn preserved_native_signal(_signum: i32) {
    SIGNAL_NATIVE_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn first_external_native_signal(_signum: i32) {
    SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn external_siginfo_signal(
    _signum: i32,
    info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    let code = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_code }
    };
    SIGNAL_EXTERNAL_SIGINFO_CODE.store(code, Ordering::SeqCst);
}

unsafe extern "C" fn external_siginfo_wait_for_replacement(
    _signum: i32,
    _info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    SIGNAL_EXTERNAL_WAIT_ENTERED.store(1, Ordering::SeqCst);
    while SIGNAL_EXTERNAL_WAIT_RELEASED.load(Ordering::SeqCst) == 0 {
        std::hint::spin_loop();
    }
}

fn wait_for_external_siginfo_code() -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let code = SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst);
        if code != i32::MIN {
            return code;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "external SA_SIGINFO handler did not observe the signal"
        );
        std::thread::yield_now();
    }
}

unsafe fn raw_sigwaitinfo(set: &libc::sigset_t, info: &mut libc::siginfo_t) -> i32 {
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigtimedwait,
            std::ptr::from_ref(set),
            std::ptr::from_mut(info),
            std::ptr::null::<libc::timespec>(),
            std::mem::size_of::<u64>(),
        ) as i32
    }
}

unsafe extern "C-unwind" fn read_current_errno() -> u64 {
    unsafe { *libc::__errno_location() as u64 }
}

unsafe extern "C-unwind" fn unblock_usr1_inside_signal_handler() {
    let mut signal: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signal);
        libc::sigaddset(&mut signal, libc::SIGUSR1);
    }
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &signal, std::ptr::null_mut()) },
        0
    );
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

unsafe extern "C-unwind" fn thunk_attempt_main_catch() {
    let ctx = super::ctx::current();
    let observed =
        if super::ctx::claim_main_panic_catch(ctx, super::ir::BuiltinCallRole::MainPanicCatcher)
            .is_none()
        {
            1
        } else {
            2
        };
    THUNK_MAIN_CATCH_RESULT.store(observed, Ordering::SeqCst);
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

unsafe extern "C-unwind" fn nested_engine_entry() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("nested Engine was not installed")
            .clone()
    });
    let observed = match unsafe { run_export(&engine, "probe", &[]) } {
        Err(error) if error.kind == RunErrorKind::EngineFault => 1,
        Err(_) => 2,
        Ok(_) => 3,
    };
    // A correctly owned EngineFault never returns to this callback: it must
    // pass through B's boundary and continue to A's outer boundary.
    FOREIGN_FAULT_BOUNDARY_RESULT.store(observed, Ordering::SeqCst);
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
            role: crate::vm::engine::ir::BuiltinCallRole::Normal,
        },
    )
}

fn engine_fault_function(name: &str, params: usize) -> FuncBody {
    function(
        name,
        params,
        RetAbi::Zst,
        Terminator::Trap("embedding contract engine fault".into()),
    )
}

fn guest_panic_with_cleanup_function() -> FuncBody {
    FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::UnwindRaise,
                    args: vec![Operand::Imm {
                        bits: 0x5151,
                        width: Width::W64,
                    }],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::engine::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&SUSPENDED_FAULT_GUEST_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "guest_panic_while_engine_fault_is_suspended".into(),
    }
}

fn returns_101_function() -> FuncBody {
    let ret = word(0);
    let mut body = function(
        "main_returns_101",
        4,
        RetAbi::Scalar(ret),
        Terminator::Return,
    );
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 101,
            width: Width::W64,
        }),
    });
    body
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

fn main_module(body: FuncBody) -> Module {
    let mut module = Module {
        funcs: vec![body].into(),
        entry: Some(EntryPlan {
            lang_start: 0,
            main_addr: LinkAddr(0),
            argc: 0,
            argv_ptr: 0,
            sigpipe: 0,
        }),
        ..Module::default()
    };
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

fn p1_identity_module(link_addr: LinkAddr, unwind: bool) -> Module {
    let ret = word(0);
    let mut body = function("p1_identity", 0, RetAbi::Scalar(ret), Terminator::Return);
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 73,
            width: Width::W64,
        }),
    });
    let sig = ForeignSig {
        args: Vec::new(),
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: Vec::new(),
        unwind,
    };
    let mut module = Module {
        funcs: vec![body].into(),
        ..Module::default()
    };
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig,
    });
    module
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

fn lifecycle_callback_library(link_addr: LinkAddr, attribute: &str) -> (NativeFixtureDir, PathBuf) {
    assert!(matches!(attribute, "constructor" | "destructor"));
    let directory = NativeFixtureDir::new(&format!("{attribute}-engine-fault"));
    let lifecycle = directory.path().join("lifecycle.c");
    let bridge = directory.path().join("bridge.S");
    let library = directory.path().join("lifecycle.so");
    std::fs::write(
        &lifecycle,
        format!(
            r#"
extern void callback(void);
__attribute__(({attribute})) static void lifecycle(void) {{ callback(); }}
"#
        ),
    )
    .unwrap();
    let slot = super::ir::native_entry_slot_name(link_addr);
    std::fs::write(
        &bridge,
        format!(
            r#"
.intel_syntax noprefix
.text
.globl callback
.hidden callback
.type callback,@function
callback:
    jmp QWORD PTR [rip + {slot}]
.size callback,.-callback
.pushsection .data.mirvm_p1,"aw",@progbits
.p2align 3
.globl {slot}
.hidden {slot}
.type {slot},@object
.size {slot},8
{slot}:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#
        ),
    )
    .unwrap();
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,-z,defs", "-Wl,-Bsymbolic"])
        .arg(&lifecycle)
        .arg(&bridge)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to build {attribute} fixture:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (directory, library)
}

fn constructor_signal_fault_archive(link_addr: LinkAddr) -> (NativeFixtureDir, PathBuf) {
    let directory = NativeFixtureDir::new("constructor-signal-fault-archive");
    let lifecycle = directory.path().join("lifecycle.c");
    let bridge = directory.path().join("bridge.S");
    let lifecycle_object = directory.path().join("lifecycle.o");
    let bridge_object = directory.path().join("bridge.o");
    let archive = directory.path().join("libconstructor_signal_fault.a");
    std::fs::write(
        &lifecycle,
        r#"
extern void callback(void);
__attribute__((constructor)) static void lifecycle(void) { callback(); }
"#,
    )
    .unwrap();
    let slot = super::ir::native_entry_slot_name(link_addr);
    std::fs::write(
        &bridge,
        format!(
            r#"
.intel_syntax noprefix
.text
.globl callback
.hidden callback
.type callback,@function
callback:
    jmp QWORD PTR [rip + {slot}]
.size callback,.-callback
.pushsection .data.mirvm_p1,"aw",@progbits
.p2align 3
.globl {slot}
.hidden {slot}
.type {slot},@object
.size {slot},8
{slot}:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#
        ),
    )
    .unwrap();
    for (source, object) in [(&lifecycle, &lifecycle_object), (&bridge, &bridge_object)] {
        let output = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(source)
            .arg("-o")
            .arg(object)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to compile constructor signal-fault fixture:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&lifecycle_object)
        .arg(&bridge_object)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to archive constructor signal-fault fixture:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let library =
        crate::native_archive::materialize_in(&archive, &directory.path().join("materialized"))
            .unwrap();
    (directory, library)
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
        crate::native_archive::materialize_in(&archive, &directory.path().join("materialized"))
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

fn signal_nested_module() -> Module {
    let mut handler = function(
        "signal_handler_calls_nested_guest",
        1,
        RetAbi::Zst,
        Terminator::Call {
            callee: 2,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::CallRole::Normal,
        },
    );
    handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });
    let nested = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![marker_store(&LIFECYCLE_SIGNAL_NESTED_RAN)],
            term: Terminator::Return,
        }],
        name: "nested_guest_from_signal".into(),
    };
    let mut module = Module {
        funcs: vec![
            calls_native(
                "close_then_record_signal",
                lifecycle_close_and_record_signal,
            ),
            handler,
            nested,
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_addrs.insert(LIFECYCLE_SIGNAL_GUEST_ADDR, 1);
    module
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
    module.fn_addrs.insert(handler_addr, 0);
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
                bits: libc::SIGUSR1 as u64,
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
                bits: libc::SIGUSR1 as u64,
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
    module.fn_addrs.insert(SIGNAL_TARGET_GUEST_ADDR, 0);
    module
}

fn physically_masked_reraising_signal_module() -> Module {
    let mut handler = function(
        "close_drain_handler_reraises_its_signal",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_OWNER_HANDLER_RAN));
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn faulting_masked_reraise_module() -> Module {
    let count = word(0);
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![Stmt::AtomicRmw {
                    op: RmwOp::Add,
                    addr: Operand::Imm {
                        bits: SIGNAL_OWNER_HANDLER_RAN.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    dst: ScalarPlace::Slot(count),
                    order: MemOrd::SeqCst,
                }],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(count)),
                    targets: vec![(0, 1)],
                    otherwise: 4,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: unblock_usr1_inside_signal_handler as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 2,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Trap(
                    "signal handler trapped after a successful same-number raise".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "fault_after_masked_same_signal_raise".into(),
    };
    let trigger = function(
        "raise_faulting_masked_signal_handler",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn constructor_faulting_masked_reraise_module(
    constructor: LinkAddr,
    handler: LinkAddr,
    library: &Path,
) -> Module {
    let count = word(0);
    let signal_handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![Stmt::AtomicRmw {
                    op: RmwOp::Add,
                    addr: Operand::Imm {
                        bits: SIGNAL_OWNER_HANDLER_RAN.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    dst: ScalarPlace::Slot(count),
                    order: MemOrd::SeqCst,
                }],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(count)),
                    targets: vec![(0, 1)],
                    otherwise: 4,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: unblock_usr1_inside_signal_handler as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 2,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Trap(
                    "constructor signal handler trapped after its masked same-number raise".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "constructor_faulting_masked_signal_handler".into(),
    };
    let install_and_raise = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostSignal,
                    args: vec![
                        Operand::Imm {
                            bits: libc::SIGUSR1 as u64,
                            width: Width::W32,
                        },
                        Operand::AddrImm(handler),
                    ],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 2,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "constructor_installs_and_raises_signal_handler".into(),
    };
    let mut module = Module {
        funcs: vec![signal_handler, install_and_raise].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.fn_addrs.insert(handler.0, 0);
    module.fn_addrs.insert(constructor.0, 1);
    module.link_fn_addrs.insert(handler, 0);
    module.link_fn_addrs.insert(constructor, 1);
    module.entry_stub_sites.extend([
        super::ir::EntryStubSite {
            link_addr: handler,
            func: 0,
            sig: signal_callback_sig(),
        },
        super::ir::EntryStubSite {
            link_addr: constructor,
            func: 1,
            sig: callback_sig(),
        },
    ]);
    module
}

fn mask_restored_faulting_signal_owner_module() -> Module {
    let count = word(0);
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![Stmt::AtomicRmw {
                    op: RmwOp::Add,
                    addr: Operand::Imm {
                        bits: SIGNAL_OWNER_HANDLER_RAN.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    dst: ScalarPlace::Slot(count),
                    order: MemOrd::SeqCst,
                }],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(count)),
                    targets: vec![(0, 1)],
                    otherwise: 2,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Trap(
                    "mask-restored same-signal callback faulted on its second delivery".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "fault_when_mask_restoration_delivers_same_signal".into(),
    };
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn close_signal_source_module() -> Module {
    let handler = function(
        "close_drain_handler_raises_cross_engine_signal",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR2 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module.fn_addrs.insert(SIGNAL_CLOSE_SOURCE_GUEST_ADDR, 0);
    module
}

fn close_signal_nine_delivery_module() -> Module {
    let count = word(0);
    let mut handler = function(
        "cross_engine_signal_reraises_until_ninth_delivery",
        1,
        RetAbi::Zst,
        Terminator::SwitchInt {
            discr: SwitchDiscr::Scalar(Operand::Slot(count)),
            targets: vec![(8, 2)],
            otherwise: 1,
        },
    );
    handler.blocks[0].stmts.push(Stmt::AtomicRmw {
        op: RmwOp::Add,
        addr: Operand::Imm {
            bits: SIGNAL_CLOSE_CHAIN_HANDLER_RAN.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        dst: ScalarPlace::Slot(count),
        order: MemOrd::SeqCst,
    });
    handler.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostRaise,
        args: vec![Operand::Imm {
            bits: libc::SIGUSR2 as u64,
            width: Width::W32,
        }],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::ir::BuiltinCallRole::Normal,
    };
    handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module.fn_addrs.insert(SIGNAL_CLOSE_CHAIN_GUEST_ADDR, 0);
    module
}

fn signal_handler_waits_for_mask_deferred_engine_close_module() -> Module {
    let handler = function(
        "signal_handler_waits_for_another_engine_close",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: lifecycle_wait_from_callback as *const () as usize as u64,
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
    let trigger = function(
        "raise_signal_that_waits_for_another_engine_close",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.fn_addrs.insert(SIGNAL_CLOSE_SOURCE_GUEST_ADDR, 0);
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
                    bits: libc::SIGUSR1 as u64,
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

fn libc_error_probe(name: &str, builtin: Builtin, args: Vec<Operand>) -> FuncBody {
    let result = word(0);
    let errno = word(8);
    FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Pair(result, errno),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin,
                    args,
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: read_current_errno as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Scalar(ScalarPlace::Slot(errno)),
                    target: 2,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(ForeignSig {
                        args: Vec::new(),
                        ret: FfiKind::U64,
                        fixed: None,
                        thunk_args: Vec::new(),
                        unwind: true,
                    }),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: name.into(),
    }
}

fn signal_libc_error_module() -> Module {
    let invalid_signal = libc_error_probe(
        "signal_invalid_signum_returns_errno",
        Builtin::HostSignal,
        vec![
            Operand::Imm {
                bits: 0,
                width: Width::W32,
            },
            Operand::Imm {
                bits: crate::os::signal::SIG_DFL as u64,
                width: Width::W64,
            },
        ],
    );
    let invalid_sigaction = libc_error_probe(
        "sigaction_invalid_signum_query_returns_errno",
        Builtin::HostSigaction,
        vec![
            Operand::Imm {
                bits: 0,
                width: Width::W32,
            },
            Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
            Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
        ],
    );
    let uncatchable_signal = libc_error_probe(
        "signal_sigkill_returns_errno",
        Builtin::HostSignal,
        vec![
            Operand::Imm {
                bits: libc::SIGKILL as u64,
                width: Width::W32,
            },
            Operand::Imm {
                bits: SIGNAL_OWNER_GUEST_ADDR,
                width: Width::W64,
            },
        ],
    );
    let realtime_signal = function(
        "realtime_guest_signal_remains_unsupported",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGRTMIN() as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_OWNER_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let handler = function(
        "signal_libc_error_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![
            invalid_signal,
            invalid_sigaction,
            uncatchable_signal,
            realtime_signal,
            handler,
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("invalid_signal".into(), 0);
    module.exports.insert("invalid_sigaction".into(), 1);
    module.exports.insert("sigkill".into(), 2);
    module.exports.insert("realtime".into(), 3);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 4);
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
                    bits: libc::SIGUSR1 as u64,
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
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn faulting_signal_p1_owner_module(link_addr: LinkAddr) -> Module {
    let handler = function(
        "cross_engine_faulting_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::Trap("cross-Engine synchronous signal handler fault".into()),
    );
    let old = word(0);
    let installer = function(
        "install_cross_engine_faulting_p1_handler",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGUSR1 as u64,
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
    let mut module = Module {
        funcs: vec![handler, installer].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn cross_engine_signal_raiser_module() -> Module {
    let trigger = function(
        "raise_another_engines_faulting_handler",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![trigger].into(),
        ..Module::default()
    };
    module.exports.insert("raise".into(), 0);
    module
}

fn native_image_signal_module(library: &Path, link_addr: LinkAddr) -> Module {
    let nested = function(
        "nested_guest_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "record_nested_guest".into(),
            sig: ForeignSig {
                args: vec![FfiKind::I32],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
            args: vec![Operand::Slot(word(8))],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let install = function(
        "install_native_image_signal_handlers",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "install_image_handlers".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: vec![(0, signal_callback_sig())],
                unwind: false,
            },
            args: vec![Operand::AddrImm(link_addr)],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let safe = function(
        "native_image_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![nested, install, safe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("safe".into(), 2);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_signal_fault_module(library: &Path, link_addr: LinkAddr) -> Module {
    let nested_fault = engine_fault_function("nested_guest_signal_engine_fault", 1);
    let install = function(
        "install_native_image_fault_signal_handlers",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "install_image_fault_handlers".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: vec![(0, signal_callback_sig())],
                unwind: false,
            },
            args: vec![Operand::AddrImm(link_addr)],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let safe = function(
        "native_image_fault_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![nested_fault, install, safe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("safe".into(), 2);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_finalizer_raise_module(library: &Path) -> Module {
    let arm = function(
        "arm_native_image_finalizer_raise",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "arm_image_finalizer_raise".into(),
            sig: ForeignSig {
                args: vec![FfiKind::I32],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let mut module = Module {
        funcs: vec![arm].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("arm".into(), 0);
    module
}

fn native_image_signal_bridge_error_module(library: &Path, link_addr: LinkAddr) -> Module {
    fn probe(
        name: &str,
        symbol: &str,
        link_addr: Option<LinkAddr>,
        returns_value: bool,
    ) -> FuncBody {
        let result = word(0);
        let args = link_addr
            .map(|address| vec![Operand::AddrImm(address)])
            .unwrap_or_default();
        function(
            name,
            0,
            if returns_value {
                RetAbi::Scalar(result)
            } else {
                RetAbi::Zst
            },
            Terminator::CallForeign {
                sym: symbol.into(),
                sig: ForeignSig {
                    args: if link_addr.is_some() {
                        vec![FfiKind::Ptr]
                    } else {
                        Vec::new()
                    },
                    ret: if returns_value {
                        FfiKind::U64
                    } else {
                        FfiKind::Void
                    },
                    fixed: None,
                    thunk_args: link_addr
                        .map(|_| vec![(0, signal_callback_sig())])
                        .unwrap_or_default(),
                    unwind: true,
                },
                args,
                ret: if returns_value {
                    RetDest::Scalar(ScalarPlace::Slot(result))
                } else {
                    RetDest::Ignore
                },
                target: 1,
                unwind: UnwindAction::Continue,
            },
        )
    }

    let handler = function(
        "native_signal_bridge_error_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    let funcs = vec![
        handler,
        probe(
            "native_signal_invalid_signum",
            "image_signal_invalid_signum",
            None,
            true,
        ),
        probe(
            "native_sigaction_invalid_signum",
            "image_sigaction_invalid_signum",
            None,
            true,
        ),
        probe(
            "native_signal_sigkill",
            "image_signal_sigkill",
            Some(link_addr),
            true,
        ),
        probe(
            "native_sigaction_sigkill",
            "image_sigaction_sigkill",
            Some(link_addr),
            true,
        ),
        probe(
            "native_signal_realtime_contract",
            "image_signal_realtime",
            Some(link_addr),
            false,
        ),
        probe(
            "native_signal_sync_fault_contract",
            "image_signal_sync_fault",
            Some(link_addr),
            false,
        ),
        probe(
            "native_sigaction_siginfo_contract",
            "image_sigaction_siginfo",
            Some(link_addr),
            false,
        ),
    ];
    let mut module = Module {
        funcs: funcs.into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    for (name, func) in [
        ("invalid_signal", 1),
        ("invalid_sigaction", 2),
        ("sigkill", 3),
        ("sigaction_sigkill", 4),
        ("realtime", 5),
        ("sync_fault", 6),
        ("siginfo", 7),
    ] {
        module.exports.insert(name.into(), func);
    }
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_closed_signal_handler_module(library: &Path, handler: u64) -> Module {
    let probe = function(
        "native_signal_closed_mirvm_handler_contract",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "image_signal_closed_handler".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: true,
            },
            args: vec![Operand::Imm {
                bits: handler,
                width: Width::W64,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let mut module = Module {
        funcs: vec![probe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("closed".into(), 0);
    module
}

fn native_oldact_reinstall_module() -> Module {
    let old = word(0);
    let mut installer = function(
        "reinstall_native_signal_oldact",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_OWNER_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    installer.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostSignal,
        args: vec![
            Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            },
            Operand::Slot(old),
        ],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::ir::BuiltinCallRole::Normal,
    };
    installer.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });

    let mut handler = function(
        "native_oldact_temporary_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let mut module = Module {
        funcs: vec![installer, handler].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 1);
    module
}

fn cross_engine_signal_reinstaller_module() -> Module {
    let old = word(0);
    let replaced = word(8);
    let mut handler = function(
        "cross_engine_middle_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OVERRIDE_HANDLER_RAN));

    let installer = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(old),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostSignal,
                    args: vec![
                        Operand::Imm {
                            bits: libc::SIGUSR1 as u64,
                            width: Width::W32,
                        },
                        Operand::Imm {
                            bits: SIGNAL_OVERRIDE_GUEST_ADDR,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(old)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostSignal,
                    args: vec![
                        Operand::Imm {
                            bits: libc::SIGUSR1 as u64,
                            width: Width::W32,
                        },
                        Operand::Slot(old),
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(replaced)),
                    target: 2,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "reinstall_another_engine_signal_oldact".into(),
    };
    let safe_point = function(
        "cross_engine_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![handler, installer, safe_point].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("probe".into(), 2);
    module.fn_addrs.insert(SIGNAL_OVERRIDE_GUEST_ADDR, 0);
    module
}

fn sigaction_mask_module() -> Module {
    let mut handler = function("sigaction_mask_handler", 1, RetAbi::Zst, Terminator::Return);
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let query = function(
        "query_sigaction_mask",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSigaction,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: 0,
                    width: Width::W64,
                },
                Operand::Slot(word(8)),
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, query].into(),
        ..Module::default()
    };
    module.exports.insert("query".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn sigaction_reinstall_module() -> Module {
    let mut handler = function(
        "raw_oldact_reinstall_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let sigaction = function(
        "reinstall_modified_raw_oldact",
        2,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSigaction,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Slot(word(8)),
                Operand::Slot(word(16)),
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, sigaction].into(),
        ..Module::default()
    };
    module.exports.insert("sigaction".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn masking_close_module(fault_after_close: bool) -> Module {
    let mut handler = function(
        "close_other_engine_while_its_signal_is_masked",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: close_nested_engine_from_signal as *const () as usize as u64,
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
    if fault_after_close {
        handler.blocks[1].term =
            Terminator::Trap("signal handler fault after requesting another Engine close".into());
    }
    let trigger = function(
        "raise_masking_close_signal",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.fn_addrs.insert(SIGNAL_MASKING_CLOSE_GUEST_ADDR, 0);
    module
}

fn masked_raise_replacement_module() -> Module {
    let mut old_handler = function(
        "masked_raise_then_replace_handler",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    old_handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_MASKED_OLD_HANDLER_RAN));
    old_handler.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostRaise,
        args: vec![Operand::Imm {
            bits: libc::SIGUSR1 as u64,
            width: Width::W32,
        }],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::ir::BuiltinCallRole::Normal,
    };
    old_handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_MASKED_NEW_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Ignore,
            target: 3,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    });
    old_handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });

    let mut new_handler = function(
        "replacement_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    new_handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_MASKED_NEW_HANDLER_RAN));
    let trigger = function(
        "raise_initial_signal_handler",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![old_handler, new_handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 2);
    module.fn_addrs.insert(SIGNAL_MASKED_OLD_GUEST_ADDR, 0);
    module.fn_addrs.insert(SIGNAL_MASKED_NEW_GUEST_ADDR, 1);
    module
}

fn raw_sigaction(handler: usize, mask: &[i32]) -> libc::sigaction {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    for &signum in mask {
        assert_eq!(unsafe { libc::sigaddset(&mut action.sa_mask, signum) }, 0);
    }
    action
}

fn process_kill_module() -> Module {
    let mut module = Module {
        funcs: vec![calls_native("process_kill_usr1", process_kill_usr1)].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
}

fn owner_fault_roundtrip_module() -> Module {
    let mut module = Module {
        funcs: vec![
            calls_native("owner_calls_nested_engine", nested_engine_entry),
            engine_fault_function("owner_engine_fault", 0),
            function(
                "owner_recovers_after_fault",
                0,
                RetAbi::Zst,
                Terminator::Return,
            ),
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.exports.insert("recover".into(), 2);
    module
}

fn foreign_fault_traversal_module(owner_fault_thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xc001;
    let catch_addr = 0xc002;
    let outer = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Scalar(result),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::CatchUnwind,
                    args: vec![
                        Operand::Imm {
                            bits: try_addr,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: catch_addr,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::engine::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_catch_frame".into(),
    };
    let try_body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: owner_fault_thunk,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_try_frame".into(),
    };
    let mut catch_body = function(
        "foreign_fault_must_not_reach_guest_catch",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0]
        .stmts
        .push(marker_store(&FOREIGN_FAULT_CATCH_RAN));

    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_addrs.insert(try_addr, 1);
    module.fn_addrs.insert(catch_addr, 2);
    module
}

fn foreign_closed_traversal_module(owner_thunk: u64) -> Module {
    let mut module = Module {
        funcs: vec![FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallIndirect {
                        callee: Operand::Imm {
                            bits: owner_thunk,
                            width: Width::W64,
                        },
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Cleanup(2),
                        null_ok: false,
                        native_sig: Some(callback_sig()),
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
                Block {
                    stmts: vec![marker_store(&FOREIGN_CLOSED_CLEANUP_RAN)],
                    term: Terminator::Resume,
                },
            ],
            name: "foreign_engine_closed_cleanup_frame".into(),
        }]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
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

fn caller_catches_owner_panic(thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xb001;
    let catch_addr = 0xb002;
    let outer = function(
        "caller_catch_unwind",
        0,
        RetAbi::Scalar(result),
        Terminator::CallBuiltin {
            builtin: Builtin::CatchUnwind,
            args: vec![
                Operand::Imm {
                    bits: try_addr,
                    width: Width::W64,
                },
                Operand::Imm {
                    bits: 0,
                    width: Width::W64,
                },
                Operand::Imm {
                    bits: catch_addr,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(result)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: crate::vm::engine::ir::BuiltinCallRole::Normal,
        },
    );
    let try_body = function(
        "caller_try_invokes_owner_thunk",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: thunk,
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
    let mut catch_body = function(
        "caller_must_not_catch_owner_exception",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0].stmts.push(Stmt::AtomicStore {
        addr: Operand::Imm {
            bits: FOREIGN_CATCH_RAN.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        order: MemOrd::SeqCst,
    });

    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_addrs.insert(try_addr, 1);
    module.fn_addrs.insert(catch_addr, 2);
    module
}

#[test]
fn guest_catch_does_not_consume_another_engines_panic() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CATCH_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("owner_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xa001, 0, &callback_sig());
        let caller = engine(caller_catches_owner_panic(thunk), jit);

        let exception = unwind::catch_raw(|| unsafe { run_export(&caller, "probe", &[]) })
            .expect_err("the owner Engine's guest panic must leave the caller Engine");
        let payload = match exception.take_mirvm(owner.shared()) {
            Ok(payload) => payload,
            Err(exception) => exception.resume_or_rethrow(),
        };
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("owner guest panic changed kind")
        };
        assert_eq!(payload.transfer(|_, inner| inner), 0x4747);
        if FOREIGN_CATCH_RAN.load(Ordering::SeqCst) != 0 {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "guest catch consumed a GuestPanic owned by another Engine in {failures:?}"
    );
}

#[test]
fn run_main_distinguishes_guest_panic_from_normal_exit_101() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let panicking = engine(main_module(guest_panic_function("main_panics", 4)), jit);
        let returning = engine(main_module(returns_101_function()), jit);
        let panic_result = run_main(&panicking);
        let return_result = run_main(&returning);

        if !matches!(panic_result, Ok(RunOutcome::GuestPanic))
            || !matches!(return_result, Ok(RunOutcome::Returned(101)))
        {
            failures.push((mode, panic_result, return_result));
        }
    }
    assert!(
        failures.is_empty(),
        "run_main collapsed guest panic and normal exit 101: {failures:?}"
    );
}

#[test]
fn run_result_has_structured_guest_panic_and_engine_fault_categories() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let guest = engine(export_module(guest_panic_function("export_panics", 0)), jit);
        let fault = engine(
            export_module(engine_fault_function("export_engine_fault", 0)),
            jit,
        );
        let guest_outcome = unsafe { run_export(&guest, "probe", &[]) }
            .expect("guest panic is a guest execution outcome");
        let fault_error = unsafe { run_export(&fault, "probe", &[]) }
            .expect_err("EngineFault must not be a successful export result");

        if guest_outcome != RunOutcome::GuestPanic || fault_error.kind != RunErrorKind::EngineFault
        {
            failures.push((mode, guest_outcome, fault_error));
        }
    }
    assert!(
        failures.is_empty(),
        "RunError has no structured guest-panic/engine-fault category: {failures:?}"
    );
}

#[test]
fn engine_fault_crosses_foreign_engine_and_is_finished_by_its_owner() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_FAULT_CATCH_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_CLEANUP_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_BOUNDARY_RESULT.store(0, Ordering::SeqCst);

        let owner = engine(owner_fault_roundtrip_module(), jit);
        let owner_fault_thunk = thunks::get_or_create(owner.shared(), 0xd001, 1, &callback_sig());
        let foreign = engine(foreign_fault_traversal_module(owner_fault_thunk), jit);

        let fault_result =
            with_nested_engine(&foreign, || unsafe { run_export(&owner, "probe", &[]) });
        let boundary_result = FOREIGN_FAULT_BOUNDARY_RESULT.load(Ordering::SeqCst);
        let catch_ran = FOREIGN_FAULT_CATCH_RAN.load(Ordering::SeqCst);
        let cleanup_ran = FOREIGN_FAULT_CLEANUP_RAN.load(Ordering::SeqCst);
        let fault_still_in_flight = super::ctx::engine_fault_in_flight();
        let recovery = unsafe { run_export(&owner, "recover", &[]) };

        let owner_classified = matches!(
            &fault_result,
            Err(error)
                if error.kind == RunErrorKind::EngineFault
                    && error.exit_code == 70
                    && error.message.contains("embedding contract engine fault")
        );
        let owner_recovered = matches!(recovery, Ok(RunOutcome::Returned(value)) if value.lo == 0);
        if !owner_classified
            || boundary_result != 0
            || catch_ran != 0
            || cleanup_ran != 0
            || fault_still_in_flight
            || !owner_recovered
        {
            failures.push(format!(
                "{mode}: fault={fault_result:?}, B-boundary={boundary_result}, \
                 B-catch={catch_ran}, B-cleanup={cleanup_ran}, \
                 in-flight={fault_still_in_flight}, recovery={recovery:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineFault changed owner or ran guest cleanup while crossing another Engine: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_does_not_hide_reentrant_guest_panic_cleanup() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        SUSPENDED_FAULT_GUEST_CLEANUP_RAN.store(0, Ordering::SeqCst);

        let fault_owner = engine(Module::default(), false);
        let activation = super::ctx::activate(fault_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "suspended by a native catch".into(), 70)
        })
        .expect_err("EngineFault must reach the native catch");

        let reentrant = engine(export_module(guest_panic_with_cleanup_function()), jit);
        let outcome = unsafe { run_export(&reentrant, "probe", &[]) };
        let cleanup_ran = SUSPENDED_FAULT_GUEST_CLEANUP_RAN.load(Ordering::SeqCst);

        let payload = suspended
            .take_mirvm(fault_owner.shared())
            .expect("the suspended EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("suspended EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(outcome, Ok(RunOutcome::GuestPanic))
            || cleanup_ran != 1
            || report.message != "suspended by a native catch"
            || super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: nested={outcome:?}, cleanup={cleanup_ran}, report={report:?}, in-flight={}",
                super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault hid cleanup for a distinct reentrant guest panic: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_allows_a_reentrant_engine_fault() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let outer_owner = engine(Module::default(), false);
        let activation = super::ctx::activate(outer_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "outer suspended EngineFault".into(), 70)
        })
        .expect_err("outer EngineFault must reach the native catch");

        let reentrant = engine(
            export_module(engine_fault_function("reentrant_engine_fault", 0)),
            jit,
        );
        let inner = unsafe { run_export(&reentrant, "probe", &[]) };
        let outer_remained_live = super::ctx::engine_fault_in_flight();

        let payload = suspended
            .take_mirvm(outer_owner.shared())
            .expect("the outer EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("outer EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(
            inner,
            Err(ref error)
                if error.kind == RunErrorKind::EngineFault
                    && error.message.contains("embedding contract engine fault")
        ) || !outer_remained_live
            || report.message != "outer suspended EngineFault"
            || super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: inner={inner:?}, outer-live={outer_remained_live}, report={report:?}, in-flight={}",
                super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault prevented an independent reentrant EngineFault: {failures:#?}"
    );
}

#[test]
fn engine_top_rethrows_host_rust_panic_unchanged() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let host = engine(
            export_module(calls_native("host_panic_export", host_panic_entry)),
            jit,
        );
        let caught = catch_unwind(AssertUnwindSafe(|| unsafe {
            run_export(&host, "probe", &[])
        }));
        match caught {
            Err(payload)
                if payload.downcast_ref::<HostPanicMarker>() == Some(&HostPanicMarker(0xe13)) => {}
            Err(_) => failures.push(format!("{mode}: host panic payload type or value changed")),
            Ok(result) => failures.push(format!(
                "{mode}: Engine boundary consumed host panic as {result:?}"
            )),
        }
        if super::ctx::engine_fault_in_flight() {
            failures.push(format!(
                "{mode}: host panic incorrectly left EngineFault state active"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine top did not preserve a host Rust panic: {failures:#?}"
    );
}

#[test]
fn interpreter_prologue_fault_restores_frame_state() {
    let broken = engine(
        export_module(function(
            "missing_required_argument",
            1,
            RetAbi::Zst,
            Terminator::Return,
        )),
        false,
    );
    let activation = super::ctx::activate(broken.shared());
    let ctx = activation.ctx();
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);

    let error = unsafe { run_export(&broken, "probe", &[]) }
        .expect_err("missing argument must be an EngineFault");
    assert_eq!(error.kind, RunErrorKind::EngineFault);
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });

    let outcome = unsafe { run_export(&broken, "probe", &[7]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });
}

#[test]
fn close_waits_for_a_real_active_call_before_teardown() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        reset_lifecycle_gate();
        let engine = engine(
            export_module(calls_native(
                "lifecycle_blocking_export",
                lifecycle_blocking_entry,
            )),
            jit,
        );
        let id = engine.shared().id;
        let execution = engine.clone();
        let running = std::thread::spawn(move || unsafe { run_export(&execution, "probe", &[]) });
        wait_lifecycle_entry();

        engine.close();
        let state_while_active = engine.state();
        let registry_while_active = super::ctx::engine(id).is_some();
        let rejected = unsafe { run_export(&engine, "probe", &[]) };
        release_lifecycle_entry();
        let completed = running.join().expect("active guest call thread panicked");
        engine.wait_closed().unwrap();

        if state_while_active != super::ctx::EngineState::Closing
            || !registry_while_active
            || !matches!(
                rejected,
                Err(ref error) if error.kind == RunErrorKind::EngineClosed
            )
            || !matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || engine.state() != super::ctx::EngineState::Closed
            || super::ctx::engine(id).is_some()
        {
            failures.push(format!(
                "{mode}: active-state={state_while_active:?}, registry={registry_while_active}, \
                 rejected={rejected:?}, completed={completed:?}, final={:?}",
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine close tore down active execution or admitted new work: {failures:#?}"
    );
}

#[test]
fn suspended_guest_exception_keeps_engine_closing_until_consumed() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("suspended_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let code = thunks::get_or_create(owner.shared(), 0xe225, 0, &callback_sig());
        let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
        let exception = unwind::catch_raw(|| unsafe { callback() })
            .expect_err("guest panic must be suspended outside the callback thunk");

        owner.close();
        let state_while_suspended = owner.state();

        let payload = exception
            .take_mirvm(owner.shared())
            .expect("suspended guest panic changed owner");
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("suspended guest panic changed kind")
        };
        let inner = payload.transfer(|_, inner| inner);
        owner.wait_closed().unwrap();

        if state_while_suspended != super::ctx::EngineState::Closing
            || owner.state() != super::ctx::EngineState::Closed
            || inner != 0x4747
        {
            failures.push(format!(
                "{mode}: suspended={state_while_suspended:?}, final={:?}, inner={inner:#x}",
                owner.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine finalized while a native catch retained its guest exception: {failures:#?}"
    );
}

#[test]
fn wait_closed_from_own_native_callback_fails_fast() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        LIFECYCLE_WAIT_RESULT.store(0, Ordering::SeqCst);
        let engine = engine(
            export_module(calls_native(
                "wait_closed_from_native_callback",
                lifecycle_wait_from_callback,
            )),
            jit,
        );

        let result = with_nested_engine(&engine, || unsafe { run_export(&engine, "probe", &[]) });
        let outer_wait = engine.wait_closed();
        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst) != 1
            || outer_wait.is_err()
            || engine.state() != super::ctx::EngineState::Closed
        {
            failures.push(format!(
                "{mode}: result={result:?}, callback-wait={}, outer-wait={outer_wait:?}, state={:?}",
                LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst),
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "wait_closed blocked on its own callback lease: {failures:#?}"
    );
}

#[test]
fn same_engine_thunk_reentry_cannot_claim_outer_main_catcher() {
    THUNK_MAIN_CATCH_RESULT.store(0, Ordering::SeqCst);
    let engine = engine(
        Module {
            funcs: vec![calls_native(
                "thunk_attempts_outer_main_catch",
                thunk_attempt_main_catch,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let code = thunks::get_or_create(engine.shared(), 0xe222, 0, &callback_sig());
    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };

    let activation = super::ctx::activate(engine.shared());
    let ctx = activation.ctx();
    let run = super::ctx::begin_main_run(ctx);
    super::ctx::call_main_panic_boundary(ctx, || {
        unsafe { callback() };
        assert_eq!(
            THUNK_MAIN_CATCH_RESULT.load(Ordering::SeqCst),
            1,
            "same-Engine thunk reentry claimed the outer main catcher"
        );
        let outer =
            super::ctx::claim_main_panic_catch(ctx, super::ir::BuiltinCallRole::MainPanicCatcher);
        assert!(
            outer.is_some(),
            "outer activation could no longer claim its catcher"
        );
    });
    assert!(!run.finish());
    drop(activation);
    engine.wait_closed().unwrap();
}

#[test]
fn closed_c_unwind_thunk_raises_structured_engine_closed() {
    let engine = engine(
        Module {
            funcs: vec![function(
                "closed_thunk_target",
                0,
                RetAbi::Zst,
                Terminator::Return,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let control = std::sync::Arc::clone(engine.control());
    let code = thunks::get_or_create(engine.shared(), 0xe220, 0, &callback_sig());
    engine.wait_closed().unwrap();

    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
    let exception = unwind::catch_raw(|| unsafe { callback() })
        .expect_err("closed C-unwind thunk must raise EngineClosed");
    exception
        .take_engine_closed(&control)
        .expect("closed thunk raised the wrong exception kind or owner");
}

#[test]
fn p1_entries_are_per_engine_and_never_aba_after_close() {
    let link_addr = LinkAddr(0x6b00_0000_0100);
    let first = engine(p1_identity_module(link_addr, true), false);
    let second = engine(p1_identity_module(link_addr, true), false);
    let first_control = std::sync::Arc::clone(first.control());
    let first_addr = first.shared().module.resolve_link_addr(link_addr);
    let second_addr = second.shared().module.resolve_link_addr(link_addr);
    assert_ne!(
        first_addr, second_addr,
        "each Engine must own a distinct P1 closure"
    );

    let first_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(first_addr as usize) };
    let second_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(second_addr as usize) };
    assert_eq!(unsafe { first_call() }, 73);
    assert_eq!(unsafe { second_call() }, 73);

    first.wait_closed().unwrap();
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must report its closed owner")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer changed owner after close");
    assert_eq!(unsafe { second_call() }, 73, "closing A must not affect B");

    let third = engine(p1_identity_module(link_addr, true), false);
    let third_addr = third.shared().module.resolve_link_addr(link_addr);
    assert_ne!(
        third_addr, first_addr,
        "P1 closure addresses must never be reused"
    );
    assert_ne!(third_addr, second_addr);
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must stay a tombstone after a new Engine opens")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer ABA-routed into the new Engine");
}

#[test]
fn constructor_guest_trap_returns_engine_initialization_error() {
    let link_addr = LinkAddr(0x6b00_0000_0300);
    let (_directory, library) = lifecycle_callback_library(link_addr, "constructor");
    let mut module = Module {
        funcs: vec![engine_fault_function("constructor_trap", 0)].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: callback_sig(),
    });

    let shared = Shared::try_from_module(module).unwrap();
    let error = match Engine::try_new(shared) {
        Ok(_) => panic!("constructor guest Trap unexpectedly initialized an Engine"),
        Err(error) => error,
    };
    assert!(
        error.contains("native constructor hit an Engine fault")
            && error.contains("embedding contract engine fault"),
        "constructor Trap returned the wrong startup error: {error}"
    );
}

#[test]
fn constructor_signal_fault_drains_masked_reraise_before_failed_engine_closes() {
    const CHILD: &str = "MIRVM_CONSTRUCTOR_SIGNAL_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        // A host embedding thread may already block an unrelated signal. That
        // makes failed startup close on its worker rather than inline, so only
        // the constructor pthread can consume its target-thread SIGUSR1 cell.
        let unrelated_mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let _unrelated_mask = unrelated_mask.block_for_handler(libc::SIGTERM).unwrap();
        let constructor = LinkAddr(0x6b00_0000_0310);
        let handler = LinkAddr(0x6b00_0000_0320);
        let (_directory, library) = constructor_signal_fault_archive(constructor);
        let module = constructor_faulting_masked_reraise_module(constructor, handler, &library);
        let mut shared = Shared::new(module);
        shared.jit.enabled = mode == "jit";
        if shared.jit.enabled {
            shared.jit.threshold = 1;
            shared.jit.sync = true;
        }

        let error = match Engine::try_new(shared) {
            Ok(_) => panic!("faulting constructor signal handler initialized an Engine"),
            Err(error) => error,
        };
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        assert!(
            error.contains("native constructor hit an Engine fault")
                && error.contains("constructor signal handler trapped"),
            "constructor signal fault returned the wrong startup error: {error}"
        );
        assert_eq!(
            SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
            2,
            "constructor EngineFault boundary did not drain the accepted same-number raise"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::engine::embed_tests::constructor_signal_fault_drains_masked_reraise_before_failed_engine_closes";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start constructor signal-fault subprocess");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        match status {
            Some(status) if status.success() => {}
            Some(status) => failures.push(format!(
                "{mode}: status={status}\n{stdout}{stderr}"
            )),
            None => failures.push(format!(
                "{mode}: constructor EngineFault left its target-thread signal pending and timed out\n{stdout}{stderr}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "constructor signal fault did not finish startup failure: {failures:#?}"
    );
}

#[test]
fn finalizer_engine_fault_aborts_inside_teardown_boundary() {
    const CHILD: &str = "MIRVM_FINALIZER_ENGINE_FAULT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let link_addr = LinkAddr(0x6b00_0000_0400);
        let (_directory, library) = lifecycle_callback_library(link_addr, "destructor");
        let mut module = Module {
            funcs: vec![engine_fault_function("finalizer_trap", 0)].into(),
            required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
            ..Module::default()
        };
        module.fn_addrs.insert(link_addr.0, 0);
        module.link_fn_addrs.insert(link_addr, 0);
        module.entry_stub_sites.push(super::ir::EntryStubSite {
            link_addr,
            func: 0,
            sig: callback_sig(),
        });

        let engine = Engine::try_new(Shared::try_from_module(module).unwrap()).unwrap();
        engine.wait_closed().unwrap();
        panic!("native finalizer EngineFault escaped teardown");
    }

    let test_name =
        "vm::engine::embed_tests::finalizer_engine_fault_aborts_inside_teardown_boundary";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start native finalizer EngineFault subprocess");
    assert!(
        !output.status.success(),
        "native finalizer EngineFault returned from teardown"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("native finalizer unwound during Engine teardown"),
        "native finalizer failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn closed_plain_c_p1_entry_aborts() {
    const CHILD: &str = "MIRVM_CLOSED_P1_C_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let link_addr = LinkAddr(0x6b00_0000_0200);
        let engine = engine(p1_identity_module(link_addr, false), false);
        let addr = engine.shared().module.resolve_link_addr(link_addr);
        engine.wait_closed().unwrap();
        let callback: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(addr as usize) };
        let _ = unsafe { callback() };
        panic!("closed plain C P1 entry returned");
    }

    let test_name = "vm::engine::embed_tests::closed_plain_c_p1_entry_aborts";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start closed plain C P1 subprocess");
    assert!(
        !output.status.success(),
        "closed plain C P1 returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plain C thunk called after its Engine was closed"),
        "closed plain C P1 failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn engine_closed_runs_cleanup_while_crossing_another_engine() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CLOSED_CLEANUP_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_cleanup_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let control = std::sync::Arc::clone(owner.control());
        let thunk = thunks::get_or_create(owner.shared(), 0xe223, 0, &callback_sig());
        owner.wait_closed().unwrap();

        let foreign = engine(foreign_closed_traversal_module(thunk), jit);
        let exception = unwind::catch_raw(|| unsafe { run_export(&foreign, "probe", &[]) })
            .expect_err("the closed owner exception must cross the foreign Engine");
        let correctly_owned = exception.take_engine_closed(&control).is_ok();
        let cleanup_ran = FOREIGN_CLOSED_CLEANUP_RAN.load(Ordering::SeqCst);
        if !correctly_owned || cleanup_ran != 1 {
            failures.push(format!(
                "{mode}: owner={correctly_owned}, foreign-cleanup={cleanup_ran}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineClosed skipped another Engine's guest cleanup: {failures:#?}"
    );
}

#[test]
fn engine_closed_obeys_a_guest_terminate_boundary() {
    const CHILD: &str = "MIRVM_ENGINE_CLOSED_TERMINATE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_terminate_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xe224, 0, &callback_sig());
        owner.wait_closed().unwrap();
        let callback: unsafe extern "C-unwind" fn() =
            unsafe { std::mem::transmute(thunk as usize) };
        unwind::guard_terminate(|| unsafe { callback() });
        panic!("EngineClosed escaped a Terminate boundary");
    }

    let test_name = "vm::engine::embed_tests::engine_closed_obeys_a_guest_terminate_boundary";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start EngineClosed Terminate subprocess");
    assert!(
        !output.status.success(),
        "Terminate child returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unwind 抵达 Terminate 边界"),
        "Terminate child failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn signal_oldact_stays_guest_visible_and_non_lifo_close_restores_native_action() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let owner = engine(
            signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let override_engine = engine(
            signal_owner_module(SIGNAL_OVERRIDE_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );

        let owner_old = super::signal::install_signal(
            owner.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();
        let mut owner_query: libc::sigaction = unsafe { std::mem::zeroed() };
        super::signal::install_sigaction(
            owner.control(),
            libc::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut owner_query) as u64,
        )
        .unwrap();

        let override_old = super::signal::install_signal(
            override_engine.control(),
            libc::SIGUSR1,
            SIGNAL_OVERRIDE_GUEST_ADDR as usize,
            Some((0, SIGNAL_OVERRIDE_GUEST_ADDR)),
        )
        .unwrap();
        owner.wait_closed().unwrap();

        let mut override_query: libc::sigaction = unsafe { std::mem::zeroed() };
        super::signal::install_sigaction(
            override_engine.control(),
            libc::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut override_query) as u64,
        )
        .unwrap();
        override_engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if owner_old != baseline.handler()
            || owner_query.sa_sigaction != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_old != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_query.sa_sigaction != SIGNAL_OVERRIDE_GUEST_ADDR as usize
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: owner-old={owner_old:#x}, owner-query={:#x}, override-old={override_old:#x}, override-query={:#x}, restored={}",
                owner_query.sa_sigaction,
                override_query.sa_sigaction,
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "guest oldact translation or non-LIFO signal restoration failed: {failures:#?}"
    );
}

#[test]
fn guest_signal_accepts_a_first_seen_external_native_handler() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(first_external_native_signal_module(), jit);
        let installed = unsafe { run_export(&engine, "install", &[]) };
        let current = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_signal_marker(&SIGNAL_FIRST_EXTERNAL_HANDLER_RAN);
        let external_ran = SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.load(Ordering::SeqCst);
        let baseline_ran = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || current.handler() != first_external_native_signal as *const () as usize
            || external_ran != 1
            || baseline_ran != 0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={installed:?}, current={:#x}, external={external_ran}, baseline={baseline_ran}, restored={}",
                current.handler(),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "guest signal rejected or failed to restore a first-seen external native handler: {failures:#?}"
    );
}

#[test]
fn guest_signal_libc_errors_return_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for export in ["invalid_signal", "invalid_sigaction", "sigkill"] {
            unsafe { *libc::__errno_location() = 0 };
            let engine = engine(signal_libc_error_module(), jit);
            let result = unsafe { run_export(&engine, export, &[]) };
            engine.wait_closed().unwrap();
            if !matches!(
                result,
                Ok(RunOutcome::Returned(value))
                    if value.lo == u64::MAX && value.hi == libc::EINVAL as u64
            ) {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "libc signal errors did not return SIG_ERR/-1 with EINVAL: {failures:#?}"
    );
}

#[test]
fn unsupported_realtime_guest_signal_remains_an_engine_fault() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(signal_libc_error_module(), jit);
        let result = unsafe { run_export(&engine, "realtime", &[]) };
        engine.wait_closed().unwrap();
        if !matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault) {
            failures.push(format!("{mode}: {result:?}"));
        }
    }

    assert!(
        failures.is_empty(),
        "unsupported realtime guest signal stopped failing loudly: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_bridge_preserves_libc_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_directory, library) = native_signal_handler_library();
    let expected = ((libc::EINVAL as u64) << 32) | 1;
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(
            native_image_signal_bridge_error_module(
                &library,
                LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR),
            ),
            jit,
        );
        for export in [
            "invalid_signal",
            "invalid_sigaction",
            "sigkill",
            "sigaction_sigkill",
        ] {
            let result = unsafe { run_export(&engine, export, &[]) };
            if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == expected) {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }
        engine.wait_closed().unwrap();
    }

    assert!(
        failures.is_empty(),
        "self-produced native signal bridge lost libc sentinel/errno results: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_bridge_raises_contract_errors_at_its_engine_boundary() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let baseline_segv = crate::os::signal::Sigaction::query(libc::SIGSEGV).unwrap();
    let baseline_realtime = crate::os::signal::Sigaction::query(libc::SIGRTMIN()).unwrap();
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for (export, message) in [
            ("realtime", "outside the supported traditional signal range"),
            ("sync_fault", "synchronous fault signal"),
            ("siginfo", "uses unsupported SA_SIGINFO"),
        ] {
            let engine = engine(
                native_image_signal_bridge_error_module(
                    &library,
                    LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR),
                ),
                jit,
            );
            let result = unsafe { run_export(&engine, export, &[]) };
            engine.wait_closed().unwrap();
            if !matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault && error.message.contains(message))
            {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }

        let link_addr = LinkAddr(0x6b00_0000_0600);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let closed_handler = owner.shared().module.resolve_link_addr(link_addr);
        owner.wait_closed().unwrap();
        let engine = engine(
            native_image_closed_signal_handler_module(&library, closed_handler),
            jit,
        );
        let result = unsafe { run_export(&engine, "closed", &[]) };
        engine.wait_closed().unwrap();
        if !matches!(
            result,
            Err(ref error)
                if error.kind == RunErrorKind::EngineFault
                    && error.message.contains("closed owner")
        ) {
            failures.push(format!("{mode}/closed: {result:?}"));
        }
    }

    let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
    let restored_segv = crate::os::signal::Sigaction::query(libc::SIGSEGV).unwrap();
    let restored_realtime = crate::os::signal::Sigaction::query(libc::SIGRTMIN()).unwrap();
    assert!(restored_usr1.same_disposition(&baseline_usr1));
    assert!(restored_segv.same_disposition(&baseline_segv));
    assert!(restored_realtime.same_disposition(&baseline_realtime));
    assert!(
        failures.is_empty(),
        "self-produced native signal bridge flattened contract errors: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_handler_runs_at_safe_point_and_can_raise_guest_handler() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore_usr1, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let (_restore_usr2, baseline_usr2) = SavedSignalDisposition::replace_with_native(libc::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let preexisting_raw = raw_sigaction(crate::os::signal::SIG_DFL, &[libc::SIGHUP]);
        let preexisting = unsafe {
            crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&preexisting_raw) as u64)
                .unwrap()
        };
        let preexisting_guard = preexisting.block_for_handler(libc::SIGWINCH).unwrap();
        let engine = engine(
            native_image_signal_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().module.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_safe_point = unsafe { read_trace() };
        let preserved_preexisting_mask = crate::os::signal::Sigaction::current_standard_mask_bits()
            .unwrap()
            & (1u64 << libc::SIGHUP)
            != 0;
        drop(preexisting_guard);
        engine.wait_closed().unwrap();
        let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let restored_usr2 = crate::os::signal::Sigaction::query(libc::SIGUSR2).unwrap();

        if !matches!(install, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_safe_point != 0
            || !matches!(safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_safe_point != 123
            || !preserved_preexisting_mask
            || !restored_usr1.same_disposition(&baseline_usr1)
            || !restored_usr2.same_disposition(&baseline_usr2)
        {
            failures.push(format!(
                "{mode}: install={install:?}, before={before_safe_point}, safe={safe:?}, after={after_safe_point}, preexisting-mask={preserved_preexisting_mask}, usr1-restored={}, usr2-restored={}",
                restored_usr1.same_disposition(&baseline_usr1),
                restored_usr2.same_disposition(&baseline_usr2),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "self-produced native signal handler did not stay out of the kernel frame: {failures:#?}"
    );
}

#[test]
fn image_native_wrapped_raise_resumes_guest_engine_fault_to_its_owner_boundary() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore_usr1, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let (_restore_usr2, baseline_usr2) = SavedSignalDisposition::replace_with_native(libc::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(
            native_image_signal_fault_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().module.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_fault = unsafe { read_trace() };
        engine.wait_closed().unwrap();
        let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let restored_usr2 = crate::os::signal::Sigaction::query(libc::SIGUSR2).unwrap();

        if !matches!(install, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_safe_point != 0
            || !matches!(
                safe,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error.message.contains("embedding contract engine fault")
            )
            || after_fault != 4
            || !restored_usr1.same_disposition(&baseline_usr1)
            || !restored_usr2.same_disposition(&baseline_usr2)
        {
            failures.push(format!(
                "{mode}: install={install:?}, before={before_safe_point}, safe={safe:?}, after={after_fault}, usr1-restored={}, usr2-restored={}",
                restored_usr1.same_disposition(&baseline_usr1),
                restored_usr2.same_disposition(&baseline_usr2),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "wrapped native raise did not return the guest EngineFault to its owner: {failures:#?}"
    );
}

#[test]
fn cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary() {
    const CHILD: &str = "MIRVM_CROSS_ENGINE_SIGNAL_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        let jit = mode == "jit";
        let owner = engine(
            faulting_signal_p1_owner_module(LinkAddr(SIGNAL_FAULTING_P1_GUEST_ADDR)),
            jit,
        );
        let raiser = engine(cross_engine_signal_raiser_module(), jit);

        let installed = unsafe { run_export(&owner, "install", &[]) };
        let raised = unsafe { run_export(&raiser, "raise", &[]) };
        let raiser_closed = raiser.wait_closed();
        let owner_closed = owner.wait_closed();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        assert!(
            matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64),
            "faulting P1 handler did not install through HostSignal: {installed:?}"
        );
        assert!(
            matches!(
                raised,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error.exit_code == 70
                        && error
                            .message
                            .contains("cross-Engine synchronous signal handler fault")
            ),
            "the raising Engine did not receive a structured handler fault: {raised:?}"
        );
        assert!(
            raiser_closed.is_ok(),
            "raising Engine could not close: {raiser_closed:?}"
        );
        assert!(
            owner_closed.is_ok(),
            "handler Engine could not close: {owner_closed:?}"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::engine::embed_tests::cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start cross-Engine signal-fault subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "cross-Engine synchronous signal fault escaped both owners: {failures:#?}"
    );
}

#[test]
fn cross_engine_mask_restored_signal_fault_returns_to_the_raising_boundary() {
    const CHILD: &str = "MIRVM_CROSS_ENGINE_MASK_RESTORED_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let owner = engine(mask_restored_faulting_signal_owner_module(), jit);
        let raiser = engine(cross_engine_signal_raiser_module(), jit);
        super::signal::install_signal(
            owner.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let raised = unsafe { run_export(&raiser, "raise", &[]) };
        let delivered = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let raiser_closed = raiser.wait_closed();
        let owner_closed = owner.wait_closed();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        assert!(
            matches!(
                raised,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error
                            .message
                            .contains("mask-restored same-signal callback faulted")
            ),
            "A did not receive B's mask-restoration fault: {raised:?}"
        );
        assert_eq!(
            delivered, 2,
            "B's successful masked reraise was not delivered exactly once"
        );
        assert!(
            raiser_closed.is_ok(),
            "raising Engine could not close: {raiser_closed:?}"
        );
        assert!(
            owner_closed.is_ok(),
            "callback Engine could not close: {owner_closed:?}"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::engine::embed_tests::cross_engine_mask_restored_signal_fault_returns_to_the_raising_boundary";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start mask-restored signal-fault subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "mask-restored cross-Engine fault escaped A's boundary: {failures:#?}"
    );
}

#[test]
fn faulting_handler_same_signal_raise_survives_cross_thread_close() {
    const CHILD: &str = "MIRVM_FAULTING_MASKED_RERAISE_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let engine = engine(faulting_masked_reraise_module(), jit);
        super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let result = unsafe { run_export(&engine, "probe", &[]) };
        let before_close = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let closer = engine.clone();
        let closed = std::thread::spawn(move || closer.wait_closed())
            .join()
            .expect("cross-thread close panicked");
        let after_close = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let native_after_close = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        assert!(
            matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault),
            "faulting signal handler returned the wrong result: {result:?}"
        );
        assert_eq!(
            before_close, 2,
            "the EngineFault boundary did not drain the accepted same-number raise"
        );
        assert!(
            closed.is_ok(),
            "cross-thread Engine close failed: {closed:?}"
        );
        assert_eq!(
            after_close, 2,
            "Engine close changed the already-drained same-number raise"
        );
        assert_eq!(
            native_after_close, 0,
            "the accepted guest raise crossed into the restored native baseline"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name =
        "vm::engine::embed_tests::faulting_handler_same_signal_raise_survives_cross_thread_close";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start faulting masked-reraise subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "same-number raise vanished after its handler faulted: {failures:#?}"
    );
}

#[test]
fn physically_blocked_native_finalizer_raise_survives_close_worker_exit() {
    const CHILD: &str = "MIRVM_MASKED_NATIVE_FINALIZER_RAISE_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(libc::SIGUSR1)
            .expect("failed to save finalizer signal disposition");
        let _restore = SavedSignalDisposition {
            signum: libc::SIGUSR1,
            action: saved,
        };
        let external = unsafe {
            let mut action = raw_sigaction(external_siginfo_signal as *const () as usize, &[]);
            action.sa_flags = libc::SA_SIGINFO;
            crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&action) as u64).unwrap()
        };
        assert_eq!(external.install(libc::SIGUSR1), 0);
        let baseline = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let (_directory, library) = native_signal_handler_library();
        SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
        let engine = engine(native_image_finalizer_raise_module(&library), mode == "jit");
        let image = &engine.shared().module.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let mask_guard = blocker.block_for_handler(libc::SIGUSR1).unwrap();
        let armed = unsafe { run_export(&engine, "arm", &[]) };
        engine.wait_closed().unwrap();
        let trace = unsafe { read_trace() };
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        drop(mask_guard);
        let native_code = wait_for_external_siginfo_code();

        assert!(matches!(armed, Ok(RunOutcome::Returned(value)) if value.lo == 0));
        assert_eq!(
            trace, 78,
            "native finalizer did not execute a successful wrapped raise"
        );
        assert_eq!(
            native_code,
            libc::SI_TKILL,
            "native finalizer changed libc raise siginfo provenance"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::engine::embed_tests::physically_blocked_native_finalizer_raise_survives_close_worker_exit";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start masked native-finalizer subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "native finalizer raise was lost with its close worker: {failures:#?}"
    );
}

#[test]
fn guest_can_reinstall_a_native_signal_oldact_and_real_kill_uses_it() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(native_oldact_reinstall_module(), jit);
        let result = unsafe { run_export(&engine, "probe", &[]) };
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_signal_marker(&SIGNAL_NATIVE_HANDLER_RAN);
        let native_ran = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        let guest_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || native_ran != 1
            || guest_ran != 0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: result={result:?}, native={native_ran}, guest={guest_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "a native oldact could not be reinstalled through guest signal(): {failures:#?}"
    );
}

#[test]
fn another_engine_can_reinstall_guest_p1_oldact_without_stealing_ownership() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OVERRIDE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let link_addr = LinkAddr(SIGNAL_OWNER_GUEST_ADDR);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let first_p1 = owner
            .shared()
            .module
            .load_map
            .resolve(link_addr)
            .expect("signal P1 entry was not materialized");
        let installer = engine(cross_engine_signal_reinstaller_module(), jit);
        let owner_install = unsafe { run_export(&owner, "install", &[]) };

        let installed = unsafe { run_export(&installer, "install", &[]) };
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !super::signal::has_engine_pending(owner.control())
            && !super::signal::has_engine_pending(installer.control())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "cross-Engine signal was not published to either Engine inbox"
            );
            std::thread::yield_now();
        }
        let owner_received = super::signal::has_engine_pending(owner.control());
        let installer_was_idle = !super::signal::has_engine_pending(installer.control());
        let installer_safe = unsafe { run_export(&installer, "probe", &[]) };
        let owner_before_safe_point = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let owner_safe = unsafe { run_export(&owner, "probe", &[]) };
        let owner_after_safe_point = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        // A closes before B even though B installed the current A callback.
        // The surviving B disposition must become current, then B's close must
        // restore the native baseline.
        owner.wait_closed().unwrap();
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&installer);
        let installer_after_owner_close = unsafe { run_export(&installer, "probe", &[]) };
        let override_after_safe_point = SIGNAL_OVERRIDE_HANDLER_RAN.load(Ordering::SeqCst);
        installer.wait_closed().unwrap();
        let target_first_restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        // Now reverse the close order. B owns both dispositions it installed,
        // including the one whose callback targets A. Closing B must reveal
        // A's original P1 registration without closing or stealing A.
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OVERRIDE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let p1 = owner
            .shared()
            .module
            .load_map
            .resolve(link_addr)
            .expect("second signal P1 entry was not materialized");
        let installer = engine(cross_engine_signal_reinstaller_module(), jit);
        let second_owner_install = unsafe { run_export(&owner, "install", &[]) };
        let original_owner_kernel = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let second_installed = unsafe { run_export(&installer, "install", &[]) };
        let installer_kernel = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        installer.wait_closed().unwrap();
        let after_installer_close = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&owner);
        let second_owner_safe = unsafe { run_export(&owner, "probe", &[]) };
        let second_owner_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        owner.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(owner_install, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == first_p1)
            || !owner_received
            || !installer_was_idle
            || !matches!(installer_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || owner_before_safe_point != 0
            || !matches!(owner_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || owner_after_safe_point != 1
            || !matches!(installer_after_owner_close, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || override_after_safe_point != 1
            || !target_first_restored.same_disposition(&baseline)
            || !matches!(second_owner_install, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || !matches!(second_installed, Ok(RunOutcome::Returned(value)) if value.lo == p1)
            || installer_kernel.same_disposition(&original_owner_kernel)
            || !after_installer_close.same_disposition(&original_owner_kernel)
            || !matches!(second_owner_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || second_owner_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: owner-install={owner_install:?}, install={installed:?}, owner-received={owner_received}, installer-idle={installer_was_idle}, owner-before={owner_before_safe_point}, owner-after={owner_after_safe_point}, override-after={override_after_safe_point}, target-first-restored={}, second-owner-install={second_owner_install:?}, second-install={second_installed:?}, installer-stub-fresh={}, installer-close-restored-owner={}, second-owner={second_owner_ran}, restored={}",
                target_first_restored.same_disposition(&baseline),
                !installer_kernel.same_disposition(&original_owner_kernel),
                after_installer_close.same_disposition(&original_owner_kernel),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "cross-Engine P1 oldact reinstall lost callback or installer ownership: {failures:#?}"
    );
}

#[test]
fn wrapped_sigaction_strips_fixed_stub_internals_from_a_modified_raw_oldact() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(sigaction_reinstall_module(), jit);
        super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let fixed_stub = baseline
            .replace(libc::SIGUSR1)
            .expect("raw sigaction did not return MIRVM's fixed-stub oldact");
        let mut modified: libc::sigaction = unsafe { std::mem::zeroed() };
        fixed_stub.write_to(std::ptr::from_mut(&mut modified) as u64);
        modified.sa_flags |= libc::SA_NOCLDSTOP;
        assert_eq!(
            unsafe { libc::sigaddset(&mut modified.sa_mask, libc::SIGUSR2) },
            0
        );
        assert_ne!(modified.sa_flags & libc::SA_SIGINFO, 0);
        assert!(modified.sa_restorer.is_some());

        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let installed = unsafe {
            run_export(
                &engine,
                "sigaction",
                &[
                    std::ptr::from_ref(&modified) as u64,
                    std::ptr::from_mut(&mut old) as u64,
                ],
            )
        };
        let mut queried: libc::sigaction = unsafe { std::mem::zeroed() };
        let query = unsafe {
            run_export(
                &engine,
                "sigaction",
                &[0, std::ptr::from_mut(&mut queried) as u64],
            )
        };
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        let usr2_masked = unsafe { libc::sigismember(&queried.sa_mask, libc::SIGUSR2) };
        if !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || !matches!(query, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old.sa_sigaction != baseline.handler()
            || queried.sa_sigaction != SIGNAL_OWNER_GUEST_ADDR as usize
            || queried.sa_flags & libc::SA_SIGINFO != 0
            || queried.sa_flags & libc::SA_NOCLDSTOP == 0
            || queried.sa_restorer.is_some()
            || usr2_masked != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={installed:?}, query={query:?}, old={:#x}, handler={:#x}, flags={:#x}, restorer={}, usr2-mask={usr2_masked}, restored={}",
                old.sa_sigaction,
                queried.sa_sigaction,
                queried.sa_flags,
                queried.sa_restorer.is_some(),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "wrapped sigaction leaked fixed-stub ABI details into the guest action: {failures:#?}"
    );
}

#[test]
fn sigaction_normalizes_uncatchable_mask_bits_and_close_restores_baseline() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(sigaction_mask_module(), jit);
        let mut action = raw_sigaction(SIGNAL_OWNER_GUEST_ADDR as usize, &[]);
        assert_eq!(unsafe { libc::sigfillset(&mut action.sa_mask) }, 0);
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let install = unsafe {
            super::signal::native_sigaction(
                libc::SIGUSR1,
                std::ptr::from_ref(&action),
                std::ptr::from_mut(&mut old),
                engine.control().id(),
            )
        };
        let mut queried: libc::sigaction = unsafe { std::mem::zeroed() };
        let query =
            unsafe { run_export(&engine, "query", &[std::ptr::from_mut(&mut queried) as u64]) };
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let safe_point = unsafe { run_export(&engine, "query", &[0]) };
        let kill_masked = unsafe { libc::sigismember(&queried.sa_mask, libc::SIGKILL) };
        let stop_masked = unsafe { libc::sigismember(&queried.sa_mask, libc::SIGSTOP) };
        let usr2_masked = unsafe { libc::sigismember(&queried.sa_mask, libc::SIGUSR2) };
        let handler_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if install != 0
            || !matches!(query, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || !matches!(safe_point, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old.sa_sigaction != baseline.handler()
            || queried.sa_sigaction != SIGNAL_OWNER_GUEST_ADDR as usize
            || kill_masked != 0
            || stop_masked != 0
            || usr2_masked != 1
            || handler_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={install}, query={query:?}, kill-mask={kill_masked}, stop-mask={stop_masked}, usr2-mask={usr2_masked}, handler={handler_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "sigaction did not normalize SIGKILL/SIGSTOP mask bits: {failures:#?}"
    );
}

#[test]
fn blocked_external_native_raise_keeps_libc_si_tkill_provenance() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let saved = crate::os::signal::Sigaction::query(libc::SIGUSR1)
        .expect("failed to save external SA_SIGINFO test disposition");
    let _restore = SavedSignalDisposition {
        signum: libc::SIGUSR1,
        action: saved,
    };
    let external = unsafe {
        let mut action = raw_sigaction(external_siginfo_signal as *const () as usize, &[]);
        action.sa_flags = libc::SA_SIGINFO;
        crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&action) as u64).unwrap()
    };
    assert_eq!(external.install(libc::SIGUSR1), 0);

    let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
    SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
    let native_guard = blocker.block_for_handler(libc::SIGUSR1).unwrap();
    assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
    assert_eq!(
        SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst),
        i32::MIN
    );
    drop(native_guard);
    let native_code = wait_for_external_siginfo_code();
    assert_eq!(native_code, libc::SI_TKILL);

    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
        let engine = engine(physically_masked_signal_module(), jit);
        let wrapped_guard = blocker.block_for_handler(libc::SIGUSR1).unwrap();
        let raised = unsafe { run_export(&engine, "raise", &[]) };
        let before_unblock = SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst);
        drop(wrapped_guard);
        let wrapped_code = wait_for_external_siginfo_code();
        engine.wait_closed().unwrap();

        if !matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_unblock != i32::MIN
            || wrapped_code != native_code
        {
            failures.push(format!(
                "{mode}: raise={raised:?}, before-unblock={before_unblock}, libc-code={native_code}, wrapped-code={wrapped_code}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise changed an external native handler's siginfo provenance: {failures:#?}"
    );
}

#[test]
fn sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state() {
    const CHILD: &str = "MIRVM_SIGWAIT_HOST_RAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_mode, jit) in modes() {
            let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
            SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
            let engine = engine(physically_masked_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                libc::SIGUSR1,
                SIGNAL_OWNER_GUEST_ADDR as usize,
                Some((0, SIGNAL_OWNER_GUEST_ADDR)),
            )
            .unwrap();

            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker.block_for_handler(libc::SIGUSR1).unwrap();
            let raised = unsafe { run_export(&engine, "raise", &[]) };

            let mut waited_set: libc::sigset_t = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::sigemptyset(&mut waited_set) }, 0);
            assert_eq!(
                unsafe { libc::sigaddset(&mut waited_set, libc::SIGUSR1) },
                0
            );
            let mut raised_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { raw_sigwaitinfo(&waited_set, &mut raised_info) },
                libc::SIGUSR1
            );
            let raised_code = raised_info.si_code;

            assert_eq!(
                unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGUSR1) },
                0
            );
            let mut killed_info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { raw_sigwaitinfo(&waited_set, &mut killed_info) },
                libc::SIGUSR1
            );
            let killed_code = killed_info.si_code;
            let safe = unsafe { run_export(&engine, "probe", &[]) };
            let handler_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
            engine.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
            drop(mask_guard);

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(raised_code, libc::SI_TKILL);
            assert_eq!(killed_code, libc::SI_TKILL);
            assert!(matches!(safe, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(
                handler_ran, 0,
                "sigwaitinfo-consumed signals reached the guest callback"
            );
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start sigwaitinfo HostRaise subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("sigwaitinfo did not consume the blocked HostRaise");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "blocked HostRaise did not preserve sigwaitinfo semantics:\n{stdout}{stderr}"
    );
}

#[test]
fn blocked_host_raise_runs_once_at_the_target_pthreads_next_safe_point() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
        let engine = engine(target_thread_signal_module(), jit);
        super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            SIGNAL_TARGET_GUEST_ADDR as usize,
            Some((0, SIGNAL_TARGET_GUEST_ADDR)),
        )
        .unwrap();

        let execution = engine.clone();
        let observed = std::thread::spawn(move || {
            let target_thread = unsafe { libc::pthread_self() } as u64;
            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker.block_for_handler(libc::SIGUSR1).unwrap();
            let raised = unsafe { run_export(&execution, "raise", &[]) };
            let while_masked = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            drop(mask_guard);
            let after_unblock = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let first_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_first_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let second_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_second_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            (
                target_thread,
                raised,
                while_masked,
                after_unblock,
                first_safe,
                after_first_safe,
                second_safe,
                after_second_safe,
            )
        })
        .join()
        .expect("target pthread panicked");
        let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(observed.1, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.2 != 0
            || observed.3 != 0
            || !matches!(observed.4, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.5 != 1
            || !matches!(observed.6, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.7 != 1
            || handler_thread != observed.0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: raise={:?}, masked={}, unblocked={}, first={:?}/{}, second={:?}/{}, target={:#x}, handler={handler_thread:#x}, restored={}",
                observed.1,
                observed.2,
                observed.3,
                observed.4,
                observed.5,
                observed.6,
                observed.7,
                observed.0,
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise did not stay with its target pthread: {failures:#?}"
    );
}

#[test]
fn raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close() {
    const CHILD: &str = "MIRVM_RAW_PTHREAD_KILL_TARGET_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        for (_mode, jit) in modes() {
            reset_lifecycle_gate();
            SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_WORKER_THREAD.store(0, Ordering::SeqCst);
            let engine = engine(target_thread_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                libc::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let execution = engine.clone();
            let running =
                std::thread::spawn(move || unsafe { run_export(&execution, "block", &[]) });
            wait_lifecycle_entry();
            let target_thread = SIGNAL_TARGET_WORKER_THREAD.load(Ordering::SeqCst);
            let sender_thread = unsafe { libc::pthread_self() } as u64;
            assert_ne!(target_thread, 0);
            assert_ne!(target_thread, sender_thread);
            assert_eq!(
                unsafe { libc::pthread_kill(target_thread as libc::pthread_t, libc::SIGUSR1) },
                0
            );
            wait_for_owner_signal_pending(&engine);

            let closer = engine.clone();
            let closed = std::thread::spawn(move || closer.wait_closed());
            release_lifecycle_entry();
            let completed = running.join().expect("target pthread panicked");
            let closed = closed.join().expect("close worker panicked");
            let handler_ran = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
            let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

            assert!(
                matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0),
                "target VM call did not return normally: {completed:?}"
            );
            assert!(closed.is_ok(), "concurrent close failed: {closed:?}");
            assert_eq!(handler_ran, 1);
            assert_eq!(handler_thread, target_thread);
            assert_ne!(handler_thread, sender_thread);
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start raw pthread_kill subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("raw pthread_kill was not drained by its target pthread");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "raw pthread_kill left its target pthread or stalled close:\n{stdout}{stderr}"
    );
}

#[test]
fn wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread() {
    const CHILD: &str = "MIRVM_WAIT_CLOSED_TARGET_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);

        for (_mode, jit) in modes() {
            reset_lifecycle_gate();
            SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
            let engine = engine(target_thread_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                libc::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (wait_tx, wait_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let target_engine = engine.clone();
            let target = std::thread::spawn(move || {
                let attached = unsafe { run_export(&target_engine, "probe", &[]) };
                ready_tx
                    .send((unsafe { libc::pthread_self() } as u64, attached))
                    .unwrap();
                wait_rx.recv().unwrap();
                result_tx.send(target_engine.wait_closed()).unwrap();
            });
            let (target_pthread, attached) = ready_rx.recv().unwrap();
            assert!(matches!(attached, Ok(RunOutcome::Returned(value)) if value.lo == 0));

            let active_engine = engine.clone();
            let active =
                std::thread::spawn(move || unsafe { run_export(&active_engine, "block", &[]) });
            wait_lifecycle_entry();

            let (checked_tx, checked_rx) = std::sync::mpsc::channel();
            let (published_tx, published_rx) = std::sync::mpsc::channel();
            super::ctx::set_wait_closed_check_hook(Box::new(move || {
                checked_tx.send(()).unwrap();
                published_rx.recv().unwrap();
            }));
            wait_tx.send(()).unwrap();
            checked_rx.recv().unwrap();
            assert_eq!(
                unsafe { libc::pthread_kill(target_pthread as libc::pthread_t, libc::SIGUSR1) },
                0
            );
            wait_for_owner_signal_pending(&engine);
            published_tx.send(()).unwrap();

            let wait_result = result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("wait_closed blocked on its current pthread's target signal");
            assert_eq!(
                wait_result,
                Err(super::ctx::WaitClosedError::ActiveOnCurrentThread)
            );
            target.join().expect("target pthread panicked");
            assert_eq!(SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(
                SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst),
                target_pthread
            );

            release_lifecycle_entry();
            let active = active.join().expect("active pthread panicked");
            assert!(matches!(active, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            engine.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(libc::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start current-pthread signal wait_closed subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wait_closed blocked on a target signal owned by its current pthread");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "current-pthread signal wait_closed regression failed:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_host_signal() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_HOST_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(libc::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: libc::SIGUSR1,
            action: saved,
        };
        let waiting = unsafe {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.sa_flags = libc::SA_SIGINFO;
            crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&action) as u64).unwrap()
        };
        assert_eq!(waiting.install(libc::SIGUSR1), 0);
        let waiting_installed = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        for (_mode, jit) in modes() {
            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let installer = engine(first_external_native_signal_module(), jit);
            let install_execution = installer.clone();
            let replacement = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let installed = unsafe { run_export(&install_execution, "install", &[]) };
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                installed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let installed = replacement
                .join()
                .expect("HostSignal replacement thread panicked");
            installer.wait_closed().unwrap();
            raiser.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "HostSignal returned the wrong old handler: {installed:?}"
            );
            assert!(restored.same_disposition(&waiting_installed));
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::external_siginfo_handler_can_wait_for_another_threads_host_signal";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler HostSignal subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("HostSignal deadlocked behind the external handler it had to release");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "external handler and HostSignal did not make progress independently:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_engine_close() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(libc::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: libc::SIGUSR1,
            action: saved,
        };
        let waiting = unsafe {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.sa_flags = libc::SA_SIGINFO;
            crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&action) as u64).unwrap()
        };

        for (_mode, jit) in modes() {
            assert_eq!(waiting.install(libc::SIGUSR1), 0);
            let waiting_installed = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
            let owner = engine(first_external_native_signal_module(), jit);
            let installed = unsafe { run_export(&owner, "install", &[]) };
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "owner did not replace the waiting handler: {installed:?}"
            );
            assert_eq!(waiting.install(libc::SIGUSR1), 0);

            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let closing_owner = owner.clone();
            let close = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let closed = closing_owner.wait_closed();
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                closed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let closed = close.join().expect("Engine close thread panicked");
            raiser.wait_closed().unwrap();
            let current = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(closed.is_ok(), "owner Engine close failed: {closed:?}");
            assert!(
                current.same_disposition(&waiting_installed),
                "owner close overwrote the raw external handler"
            );
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::external_siginfo_handler_can_wait_for_another_threads_engine_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Engine close deadlocked behind the external handler waiting for it");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "external handler and Engine close did not make progress independently:\n{stdout}{stderr}"
    );
}

#[test]
fn preexisting_pthread_mask_blocks_wrapped_raise_and_deferred_inbox() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(physically_masked_signal_module(), jit);
        super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let raise_guard = mask.block_for_handler(libc::SIGUSR1).unwrap();
        let raised = unsafe { run_export(&engine, "raise", &[]) };
        let raised_while_masked = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        drop(raise_guard);
        let raised_safe = unsafe { run_export(&engine, "probe", &[]) };
        let raised_after_unmask = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let inbox_guard = mask.block_for_handler(libc::SIGUSR1).unwrap();
        let inbox_masked_safe = unsafe { run_export(&engine, "probe", &[]) };
        let inbox_while_masked = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        drop(inbox_guard);
        let inbox_unmasked_safe = unsafe { run_export(&engine, "probe", &[]) };
        let inbox_after_unmask = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        if !matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || raised_while_masked != 0
            || !matches!(raised_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || raised_after_unmask != 1
            || !matches!(inbox_masked_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || inbox_while_masked != 0
            || !matches!(inbox_unmasked_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || inbox_after_unmask != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: raise={raised:?}, raise-masked={raised_while_masked}, raise-safe={raised_safe:?}, raise-after={raised_after_unmask}, inbox-masked-safe={inbox_masked_safe:?}, inbox-masked={inbox_while_masked}, inbox-safe={inbox_unmasked_safe:?}, inbox-after={inbox_after_unmask}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "the VM ignored the calling pthread's preexisting signal mask: {failures:#?}"
    );
}

#[test]
fn physically_masked_inbox_event_does_not_deadlock_engine_close() {
    const CHILD: &str = "MIRVM_PHYSICALLY_MASKED_SIGNAL_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);

        for (_mode, jit) in modes() {
            for wrapped_raise in [false, true] {
                SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
                SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
                let engine = engine(physically_masked_signal_module(), jit);
                super::signal::install_signal(
                    engine.control(),
                    libc::SIGUSR1,
                    SIGNAL_OWNER_GUEST_ADDR as usize,
                    Some((0, SIGNAL_OWNER_GUEST_ADDR)),
                )
                .unwrap();

                let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
                let mask_guard = if wrapped_raise {
                    let mask_guard = mask.block_for_handler(libc::SIGUSR1).unwrap();
                    let raised = unsafe { run_export(&engine, "raise", &[]) };
                    assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
                    assert!(!super::signal::has_engine_pending(engine.control()));
                    mask_guard
                } else {
                    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
                    wait_for_owner_signal_pending(&engine);
                    mask.block_for_handler(libc::SIGUSR1).unwrap()
                };

                physically_masked_close_child_finish(engine, mask_guard, &baseline, wrapped_raise);
            }
        }
        return;
    }

    let test_name =
        "vm::engine::embed_tests::physically_masked_inbox_event_does_not_deadlock_engine_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start physically masked signal close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine with a physically masked inbox event did not terminate");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "physically masked signal close child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn close_drain_does_not_lose_a_handler_reraise_on_its_temporary_thread() {
    const CHILD: &str = "MIRVM_CLOSE_DRAIN_RERAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);

        for (_mode, jit) in modes() {
            SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
            let engine = engine(physically_masked_reraising_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                libc::SIGUSR1,
                SIGNAL_OWNER_GUEST_ADDR as usize,
                Some((0, SIGNAL_OWNER_GUEST_ADDR)),
            )
            .unwrap();

            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
            wait_for_owner_signal_pending(&engine);
            let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = mask.block_for_handler(libc::SIGUSR1).unwrap();
            engine.wait_closed().unwrap();
            assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(
                SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst),
                1,
                "close lost a signal reraised by the handler on its temporary finalizer thread"
            );
            assert_ne!(
                crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
                    & (1u64 << libc::SIGUSR1),
                0,
                "Engine close changed the caller's preexisting pthread mask"
            );
            assert!(
                crate::os::signal::Sigaction::query(libc::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
            drop(mask_guard);
            assert_eq!(SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst), 1);
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::close_drain_does_not_lose_a_handler_reraise_on_its_temporary_thread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start close-drain reraised-signal subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine lost or stalled a signal reraised by its handler");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "close-drain reraised-signal child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn close_drain_exhausts_cross_engine_synchronous_raises_before_sealing() {
    const CHILD: &str = "MIRVM_CLOSE_DRAIN_NINTH_RAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore_usr1, baseline_usr1) =
            SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        let (_restore_usr2, baseline_usr2) =
            SavedSignalDisposition::replace_with_native(libc::SIGUSR2);

        for (_mode, jit) in modes() {
            SIGNAL_CLOSE_CHAIN_HANDLER_RAN.store(0, Ordering::SeqCst);
            let source = engine(close_signal_source_module(), jit);
            let chain = engine(close_signal_nine_delivery_module(), jit);
            super::signal::install_signal(
                source.control(),
                libc::SIGUSR1,
                SIGNAL_CLOSE_SOURCE_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_SOURCE_GUEST_ADDR)),
            )
            .unwrap();
            super::signal::install_signal(
                chain.control(),
                libc::SIGUSR2,
                SIGNAL_CLOSE_CHAIN_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_CHAIN_GUEST_ADDR)),
            )
            .unwrap();

            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
            wait_for_owner_signal_pending(&source);
            let raw = raw_sigaction(crate::os::signal::SIG_DFL, &[libc::SIGUSR2]);
            let mask = unsafe {
                crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&raw) as u64).unwrap()
            };
            let mask_guard = mask.block_for_handler(libc::SIGUSR1).unwrap();
            source.wait_closed().unwrap();
            assert_eq!(
                SIGNAL_CLOSE_CHAIN_HANDLER_RAN.load(Ordering::SeqCst),
                9,
                "close sealed while a ninth cross-Engine synchronous raise was still pending"
            );
            assert_eq!(source.state(), super::ctx::EngineState::Closed);
            assert_eq!(chain.state(), super::ctx::EngineState::Running);
            let physical = crate::os::signal::Sigaction::current_standard_mask_bits().unwrap();
            assert_ne!(physical & (1u64 << libc::SIGUSR1), 0);
            assert_ne!(physical & (1u64 << libc::SIGUSR2), 0);
            drop(mask_guard);
            chain.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(libc::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline_usr1)
            );
            assert!(
                crate::os::signal::Sigaction::query(libc::SIGUSR2)
                    .unwrap()
                    .same_disposition(&baseline_usr2)
            );
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::close_drain_exhausts_cross_engine_synchronous_raises_before_sealing";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start ninth close-drain raise subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("close did not finish its finite cross-Engine synchronous raise chain");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "ninth close-drain raise child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn wait_closed_fails_fast_for_a_finalizer_deferred_by_the_current_signal_mask() {
    const CHILD: &str = "MIRVM_MASK_DEFERRED_WAIT_CLOSED_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);

        for (_mode, jit) in modes() {
            LIFECYCLE_WAIT_RESULT.store(0, Ordering::SeqCst);
            let target = engine(Module::default(), jit);
            let caller = engine(
                signal_handler_waits_for_mask_deferred_engine_close_module(),
                jit,
            );
            super::signal::install_signal(
                caller.control(),
                libc::SIGUSR1,
                SIGNAL_CLOSE_SOURCE_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_SOURCE_GUEST_ADDR)),
            )
            .unwrap();

            let result =
                with_nested_engine(&target, || unsafe { run_export(&caller, "probe", &[]) });
            assert!(matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(
                LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst),
                1,
                "wait_closed did not report its current-thread deferred finalizer"
            );
            target.wait_closed().unwrap();
            assert_eq!(target.state(), super::ctx::EngineState::Closed);
            caller.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(libc::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::wait_closed_fails_fast_for_a_finalizer_deferred_by_the_current_signal_mask";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start mask-deferred wait_closed subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wait_closed blocked on a finalizer deferred by the current signal mask");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "mask-deferred wait_closed child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn masked_raise_coalesces_and_uses_the_disposition_current_at_unmask() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_MASKED_OLD_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_MASKED_NEW_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(masked_raise_replacement_module(), jit);
        let old = super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            SIGNAL_MASKED_OLD_GUEST_ADDR as usize,
            Some((0, SIGNAL_MASKED_OLD_GUEST_ADDR)),
        )
        .unwrap();
        let result = unsafe { run_export(&engine, "probe", &[]) };
        let old_ran = SIGNAL_MASKED_OLD_HANDLER_RAN.load(Ordering::SeqCst);
        let new_ran = SIGNAL_MASKED_NEW_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if old != baseline.handler()
            || !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old_ran != 1
            || new_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: old={old:#x}, result={result:?}, old-ran={old_ran}, new-ran={new_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "masked raise stayed bound to the old handler or did not coalesce: {failures:#?}"
    );
}

#[test]
fn masked_cross_engine_close_hands_finalization_to_an_unmasked_thread() {
    const CHILD: &str = "MIRVM_MASKED_SIGNAL_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore_usr1, baseline_usr1) =
            SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        let (_restore_usr2, baseline_usr2) =
            SavedSignalDisposition::replace_with_native(libc::SIGUSR2);

        for (_mode, jit) in modes() {
            for fault_after_close in [false, true] {
                SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
                SIGNAL_MASKED_CLOSE_RETURNED.store(0, Ordering::SeqCst);
                let pending_owner = engine(
                    signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
                    jit,
                );
                let masking_owner = engine(masking_close_module(fault_after_close), jit);
                super::signal::install_signal(
                    pending_owner.control(),
                    libc::SIGUSR2,
                    SIGNAL_OWNER_GUEST_ADDR as usize,
                    Some((0, SIGNAL_OWNER_GUEST_ADDR)),
                )
                .unwrap();
                let raw = raw_sigaction(SIGNAL_MASKING_CLOSE_GUEST_ADDR as usize, &[libc::SIGUSR2]);
                let action = unsafe {
                    crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&raw) as u64)
                        .unwrap()
                };
                super::signal::install_sigaction(
                    masking_owner.control(),
                    libc::SIGUSR1,
                    Some(action),
                    Some((0, SIGNAL_MASKING_CLOSE_GUEST_ADDR)),
                    0,
                )
                .unwrap();

                assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR2) }, 0);
                wait_for_owner_signal_pending(&pending_owner);
                let result = with_nested_engine(&pending_owner, || unsafe {
                    run_export(&masking_owner, "probe", &[])
                });
                pending_owner.wait_closed().unwrap();
                if fault_after_close {
                    assert!(matches!(
                        result,
                        Err(ref error) if error.kind == RunErrorKind::EngineFault
                    ));
                } else {
                    assert!(matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0));
                }
                assert_eq!(SIGNAL_MASKED_CLOSE_RETURNED.load(Ordering::SeqCst), 1);
                assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
                assert_eq!(pending_owner.state(), super::ctx::EngineState::Closed);
                masking_owner.wait_closed().unwrap();
                assert!(
                    crate::os::signal::Sigaction::query(libc::SIGUSR1)
                        .unwrap()
                        .same_disposition(&baseline_usr1)
                );
                assert!(
                    crate::os::signal::Sigaction::query(libc::SIGUSR2)
                        .unwrap()
                        .same_disposition(&baseline_usr2)
                );
            }
        }
        return;
    }

    let test_name = "vm::engine::embed_tests::masked_cross_engine_close_hands_finalization_to_an_unmasked_thread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start masked signal close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine under another handler's mask did not terminate");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "masked signal close child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn process_signal_waits_for_its_inactive_owner_while_another_engine_runs() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let active_foreign = engine(process_kill_module(), jit);
        super::signal::install_signal(
            owner.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let foreign_result = with_nested_engine(&owner, || unsafe {
            run_export(&active_foreign, "probe", &[])
        });
        let after_foreign = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let owner_result = unsafe { run_export(&owner, "probe", &[]) };
        let after_owner = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        active_foreign.wait_closed().unwrap();
        owner.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(foreign_result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_foreign != 0
            || !matches!(owner_result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_owner != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: foreign={foreign_result:?}, after-foreign={after_foreign}, owner={owner_result:?}, after-owner={after_owner}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "process signal ran in the wrong Engine activation: {failures:#?}"
    );
}

#[test]
fn closing_engine_defers_signal_until_a_safe_point_without_jit_lock_reentry() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        LIFECYCLE_SIGNAL_NESTED_RAN.store(0, Ordering::SeqCst);
        LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.store(u64::MAX, Ordering::SeqCst);
        let engine = engine(signal_nested_module(), jit);
        let old = super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
            LIFECYCLE_SIGNAL_GUEST_ADDR as usize,
            Some((1, LIFECYCLE_SIGNAL_GUEST_ADDR)),
        )
        .unwrap();

        let result = with_nested_engine(&engine, || unsafe { run_export(&engine, "probe", &[]) });
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old != baseline.handler()
            || LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.load(Ordering::SeqCst) != 0
            || LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst) != 1
            || engine.state() != super::ctx::EngineState::Closed
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: result={result:?}, old={old:#x}, before_native_return={}, nested={}, state={:?}, restored={}",
                LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.load(Ordering::SeqCst),
                LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst),
                engine.state(),
                restored.same_disposition(&baseline),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "closing Engine did not defer signal delivery to an unlocked VM safe point: {failures:#?}"
    );
}
