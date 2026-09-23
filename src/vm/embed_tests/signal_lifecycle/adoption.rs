//! Adopting a first-seen external native handler into a guest disposition.

use super::*;

#[test]
fn guest_signal_accepts_a_first_seen_external_native_handler() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(first_external_native_signal_module(), jit);
        let installed = unsafe { run_export(&engine, "install", &[]) };
        let current = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_signal_marker(&SIGNAL_FIRST_EXTERNAL_HANDLER_RAN);
        let external_ran = SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.load(Ordering::SeqCst);
        let baseline_ran = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

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
