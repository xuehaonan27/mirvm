#![feature(rustc_private)]

use mirvm::pack::Package;
use mirvm::vm::engine::raw::run_export_raw;

fn export(engine: &mirvm::vm::engine::Engine, name: &str) -> u64 {
    unsafe { run_export_raw(engine, name, &[]) }
        .unwrap_or_else(|error| panic!("{name} export failed: {error}"))
        .into_returned()
        .unwrap_or_else(|| panic!("{name} unexpectedly panicked"))
        .lo
}

fn main() {
    let path = std::env::args().nth(1).expect("package path");
    let fini_log =
        std::env::temp_dir().join(format!("mirvm-package-fini-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&fini_log);
    unsafe { std::env::set_var("MIRVM_FINI_LOG", &fini_log) };
    let package = std::sync::Arc::new(Package::load(&path).expect("validated package"));
    std::fs::write(&path, b"changed after Package::load").expect("replace source package");
    std::fs::remove_file(&path).expect("delete source package");

    let launch = |name: &'static str| {
        let package = std::sync::Arc::clone(&package);
        std::thread::spawn(move || {
            // SAFETY: this test built the package and owns its native/FFI ABI contract.
            unsafe { package.instantiate() }.expect(name)
        })
    };
    let first = launch("first Engine");
    let second = launch("second Engine");
    let first = first.join().expect("first thread");
    let second = second.join().expect("second thread");

    let first_ptr = export(&first, "callback_ptr");
    let second_ptr = export(&second, "callback_ptr");
    let first_ctor = export(&first, "constructor_value");
    let second_ctor = export(&second, "constructor_value");
    let first_callback: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(first_ptr as usize) };
    let second_callback: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(second_ptr as usize) };
    let a1 = first
        .catch_callback_unwind(|| unsafe { first_callback() })
        .expect("first callback while live");
    let b1 = second
        .catch_callback_unwind(|| unsafe { second_callback() })
        .expect("second callback while live");
    let a2 = export(&first, "through_bridge");
    let b2 = export(&second, "through_bridge");
    let a3 = export(&first, "through_c2");
    let b3 = export(&second, "through_c2");

    first.wait_closed().expect("close first Engine");
    let first_fini =
        std::fs::read_to_string(&fini_log).expect("first native finalizer log") == "1\n";
    let closed = first
        .catch_callback_unwind(|| unsafe { first_callback() })
        .is_err();
    let live = second
        .catch_callback_unwind(|| unsafe { second_callback() })
        .expect("second callback remains live");

    // SAFETY: same trusted immutable package, instantiated after the first Engine closed.
    let third = unsafe { package.instantiate() }.expect("third Engine");
    let third_ptr = export(&third, "callback_ptr");
    let third_ctor = export(&third, "constructor_value");
    let third_callback: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(third_ptr as usize) };
    let fresh = third
        .catch_callback_unwind(|| unsafe { third_callback() })
        .expect("third callback");
    let aba = third_ptr != first_ptr
        && third_ptr != second_ptr
        && first
            .catch_callback_unwind(|| unsafe { first_callback() })
            .is_err();
    second.wait_closed().expect("close second Engine");
    third.wait_closed().expect("close third Engine");
    let fini = first_fini
        && std::fs::read_to_string(&fini_log).expect("all native finalizer log") == "1\n1\n1\n";
    let _ = std::fs::remove_file(&fini_log);
    println!(
        "unique={} ctor={first_ctor},{second_ctor} direct={a1},{b1} bridge={a2},{b2} c2={a3},{b3} live={live} fresh_ctor={third_ctor} fresh={fresh} closed={closed} aba={aba} fini={fini}",
        first_ptr != second_ptr
    );
}
