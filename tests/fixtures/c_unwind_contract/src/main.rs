use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

unsafe extern "C-unwind" {
    fn cpp_reset_caught();
    fn cpp_caught_value() -> i32;
    fn cpp_no_throw() -> i32;
    fn cpp_throw_marker();
    fn cpp_call_typed_catch(callback: unsafe extern "C-unwind" fn()) -> i32;
    fn cpp_call_catch_rethrow(callback: unsafe extern "C-unwind" fn());
    fn cpp_call_catch_swallow(callback: unsafe extern "C-unwind" fn()) -> i32;
    fn cpp_call_no_catch(callback: unsafe extern "C-unwind" fn());
}

unsafe extern "C" {
    fn cpp_call_plain_c(callback: unsafe extern "C" fn()) -> i32;
}

static DROPS: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

fn reset() {
    DROPS.store(0, Ordering::SeqCst);
    ARMED.store(false, Ordering::SeqCst);
    unsafe { cpp_reset_caught() };
}

#[inline(never)]
fn no_throw_with_cleanup() -> i32 {
    let _guard = Guard;
    unsafe { cpp_no_throw() }
}

unsafe extern "C-unwind" fn throw_foreign_from_guest() {
    let _guard = Guard;
    unsafe { cpp_throw_marker() };
}

unsafe extern "C-unwind" fn panic_from_guest() {
    let _guard = Guard;
    panic::resume_unwind(Box::new(51_i32));
}

unsafe extern "C" fn panic_from_plain_c_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    let _guard = Guard;
    panic::resume_unwind(Box::new(61_i32));
}

unsafe extern "C" fn foreign_through_plain_c_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { cpp_throw_marker() };
}

unsafe extern "C" fn foreign_indirect_through_plain_c_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    let throw: unsafe extern "C-unwind" fn() = cpp_throw_marker;
    unsafe { throw() };
}

unsafe extern "C" fn nested_panic_through_plain_c_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { cpp_call_no_catch(panic_from_guest) };
}

unsafe extern "C" fn nested_panic_indirect_through_plain_c_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    let call: unsafe extern "C-unwind" fn(unsafe extern "C-unwind" fn()) = cpp_call_no_catch;
    unsafe { call(panic_from_guest) };
}

unsafe extern "C-unwind" fn panic_swallowed_from_guest() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }
    let _guard = Guard;
    panic::resume_unwind(Box::new(71_i32));
}

fn main() {
    let mode = std::env::args().nth(1).expect("mode");
    reset();
    match mode.as_str() {
        "no-throw" => {
            let mut value = 0;
            for _ in 0..30_000 {
                value = no_throw_with_cleanup();
            }
            println!("value={value} drops={}", DROPS.load(Ordering::SeqCst));
        }
        "typed-catch" => unsafe {
            let result = cpp_call_typed_catch(throw_foreign_from_guest);
            println!(
                "result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "foreign-at-catch-unwind" => {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe { cpp_throw_marker() }));
            println!("unexpected catch_unwind return");
        }
        "panic-rethrow" => {
            let caught = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                cpp_call_catch_rethrow(panic_from_guest)
            }));
            let payload = caught
                .expect_err("C++ must rethrow the Rust panic")
                .downcast::<i32>()
                .expect("original Rust panic payload");
            println!(
                "payload={} caught={} drops={}",
                *payload,
                unsafe { cpp_caught_value() },
                DROPS.load(Ordering::SeqCst)
            );
        }
        "panic-swallow" => unsafe {
            assert_eq!(cpp_call_catch_swallow(panic_swallowed_from_guest), 0);
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_catch_swallow(panic_swallowed_from_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "plain-c-panic" => unsafe {
            assert_eq!(cpp_call_plain_c(panic_from_plain_c_guest), 0);
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_plain_c(panic_from_plain_c_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "plain-c-foreign" => unsafe {
            assert_eq!(cpp_call_plain_c(foreign_through_plain_c_guest), 0);
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_plain_c(foreign_through_plain_c_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "plain-c-foreign-indirect" => unsafe {
            assert_eq!(cpp_call_plain_c(foreign_indirect_through_plain_c_guest), 0);
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_plain_c(foreign_indirect_through_plain_c_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "plain-c-nested-panic" => unsafe {
            assert_eq!(cpp_call_plain_c(nested_panic_through_plain_c_guest), 0);
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_plain_c(nested_panic_through_plain_c_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        "plain-c-nested-panic-indirect" => unsafe {
            assert_eq!(
                cpp_call_plain_c(nested_panic_indirect_through_plain_c_guest),
                0
            );
            ARMED.store(true, Ordering::SeqCst);
            let result = cpp_call_plain_c(nested_panic_indirect_through_plain_c_guest);
            println!(
                "unexpected result={result} caught={} drops={}",
                cpp_caught_value(),
                DROPS.load(Ordering::SeqCst)
            );
        },
        _ => panic!("unknown mode `{mode}`"),
    }
}
