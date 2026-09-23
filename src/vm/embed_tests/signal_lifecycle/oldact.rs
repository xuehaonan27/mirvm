//! Old-action visibility: what a guest sees returned and what close restores.

use super::*;

#[test]
fn signal_oldact_stays_guest_visible_and_non_lifo_close_restores_native_action() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
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

        let owner_old = super::super::signal::install_signal(
            owner.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();
        let mut owner_query = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        super::super::signal::install_sigaction(
            owner.control(),
            crate::os::signal::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut owner_query) as u64,
        )
        .unwrap();

        let override_old = super::super::signal::install_signal(
            override_engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OVERRIDE_GUEST_ADDR as usize,
            Some((0, SIGNAL_OVERRIDE_GUEST_ADDR)),
        )
        .unwrap();
        owner.wait_closed().unwrap();

        let mut override_query = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        super::super::signal::install_sigaction(
            override_engine.control(),
            crate::os::signal::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut override_query) as u64,
        )
        .unwrap();
        override_engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if owner_old != baseline.handler()
            || owner_query.handler() != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_old != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_query.handler() != SIGNAL_OVERRIDE_GUEST_ADDR as usize
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: owner-old={owner_old:#x}, owner-query={:#x}, override-old={override_old:#x}, override-query={:#x}, restored={}",
                owner_query.handler(),
                override_query.handler(),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "guest oldact translation or non-LIFO signal restoration failed: {failures:#?}"
    );
}
