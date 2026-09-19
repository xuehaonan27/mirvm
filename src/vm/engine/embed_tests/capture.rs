//! Capture and telemetry tests: host-syscall result/errno parity, and the
//! automatic host-syscall rewrite recorded by a `CaptureSession`.

use super::*;

fn host_syscall_module(builtin: Builtin, nr: i64, args: &[u64]) -> Module {
    let result = word(0);
    let errno = word(8);
    let mut operands = Vec::with_capacity(args.len() + 1);
    operands.push(Operand::Imm {
        bits: nr as u64,
        width: Width::W64,
    });
    operands.extend(args.iter().copied().map(|bits| Operand::Imm {
        bits,
        width: Width::W64,
    }));
    export_module(FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Pair(result, errno),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: builtin.clone(),
                    args: vec![Operand::Imm {
                        bits: u64::MAX,
                        width: Width::W64,
                    }],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: crate::vm::engine::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin,
                    args: operands,
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 2,
                    unwind: UnwindAction::Continue,
                    role: crate::vm::engine::ir::BuiltinCallRole::Normal,
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
                    target: 3,
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
        name: "host_syscall".into(),
    })
}

#[test]
fn host_syscall_variants_preserve_libc_result_and_errno() {
    let mut failures = Vec::new();
    for (path, builtin) in [
        ("plain", Builtin::HostSyscall),
        ("trace", Builtin::HostSyscallTrace),
    ] {
        for (mode, jit) in modes() {
            let success_engine = engine(
                host_syscall_module(builtin.clone(), libc::SYS_getpid, &[]),
                jit,
            );
            let success = unsafe { run_export(&success_engine, "probe", &[]) };
            if jit && success_engine.shared().jit.slots[0].load(Ordering::Acquire) == 0 {
                failures.push(format!("{path}/{mode}/success: function did not compile"));
            }
            success_engine.wait_closed().unwrap();
            if !matches!(
                success,
                Ok(RunOutcome::Returned(value))
                    if value.lo == unsafe { libc::getpid() } as u64
                        && value.hi == libc::ENOSYS as u64
            ) {
                failures.push(format!("{path}/{mode}/success: result={success:?}"));
            }

            let failure_engine = engine(host_syscall_module(builtin.clone(), -1, &[]), jit);
            let failure = unsafe { run_export(&failure_engine, "probe", &[]) };
            if jit && failure_engine.shared().jit.slots[0].load(Ordering::Acquire) == 0 {
                failures.push(format!("{path}/{mode}/failure: function did not compile"));
            }
            failure_engine.wait_closed().unwrap();
            if !matches!(
                failure,
                Ok(RunOutcome::Returned(value))
                    if value.lo == u64::MAX && value.hi == libc::ENOSYS as u64
            ) {
                failures.push(format!("{path}/{mode}/failure: result={failure:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "HostSyscall changed libc result/errno semantics: {failures:#?}"
    );
}

#[test]
fn capture_session_records_automatic_host_syscall_rewrite() {
    const CHILD_ENV: &str = "MIRVM_ENGINE_CAPTURE_E2E_CHILD";
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        let output = PathBuf::from(output);
        let mut session = crate::telemetry::capture::CaptureSession::start(
            crate::telemetry::capture::StartOptions::new(&output, 16 << 10),
        )
        .unwrap();
        let mut engine_ids = Vec::new();
        for (mode, jit) in modes() {
            let engine = engine(
                host_syscall_module(Builtin::HostSyscall, libc::SYS_getpid, &[]),
                jit,
            );
            assert_eq!(
                engine.shared().domain,
                crate::vm::engine::jit::CodeDomain::Trace,
                "{mode} Engine stayed plain"
            );
            engine_ids.push(engine.shared().id);
            let result = unsafe { run_export(&engine, "probe", &[]) };
            assert!(
                matches!(
                    result,
                    Ok(RunOutcome::Returned(value))
                        if value.lo == unsafe { libc::getpid() } as u64
                            && value.hi == libc::ENOSYS as u64
                ),
                "{mode} result changed under capture: {result:?}"
            );
            if jit {
                // The Engine is in the trace domain, so its compiled body is
                // published into the trace slots; dispatch picks that set on its
                // own, which is what makes this a trace-domain run at all.
                let domain = engine.shared().domain;
                assert_ne!(
                    engine.shared().jit.slots_for(domain).slots[0].load(Ordering::Acquire),
                    0,
                    "trace function was not JIT-published in its own domain"
                );
                // Trace compiled code is the only caller of the pinned syscall
                // helper, so a non-zero count is what separates "the body ran
                // through the register the boundary installed" from "the
                // interpreter recorded the same bytes through TLS".
                assert!(
                    crate::vm::engine::jit::stat_value("syscall_trace") > 0,
                    "the trace Engine never reached its pinned syscall site"
                );
            }
            engine.wait_closed().unwrap();
        }
        let crate::telemetry::capture::FinishStatus::Finished(summary) =
            session.finish(std::time::Duration::from_secs(2)).unwrap()
        else {
            panic!("capture writer did not finish");
        };
        assert_eq!(summary.encoded_records, (engine_ids.len() * 4) as u64);

        let mut events = Vec::new();
        let outcome = crate::telemetry::decode::decode_file(&output, &mut |event| {
            events.push(event.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            outcome.health,
            crate::telemetry::decode::Health::Clean,
            "{:?}",
            outcome.issues
        );
        assert_eq!(events.len(), engine_ids.len() * 4);
        for engine_id in engine_ids {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.context.engine_id == Some(engine_id))
                    .count(),
                4
            );
        }
        return;
    }

    let id = NEXT_NATIVE_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let output = std::env::temp_dir().join(format!(
        "mirvm-engine-capture-{}-{id}.mlog",
        std::process::id()
    ));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("vm::engine::embed_tests::capture::capture_session_records_automatic_host_syscall_rewrite")
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        // The child must count helper entries: the assertion that the trace
        // domain reached its pinned syscall site reads that counter.
        .env(crate::options::env_var_name("jit_stats"), "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Engine capture subprocess did not finish within five seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success());
    std::fs::remove_file(output).unwrap();
}
