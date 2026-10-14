//! Guest execution, stack reservation and process-exit ownership.

/// Engine entry: by default runs the main startup chain; `--vm-call 'name(args…)'` calls an
/// exported function directly (gate entry); `--vm-stats` prints Trap-debt statistics.
/// `--stack-size` / `MIRVM_STACK_SIZE` parse a byte count with an optional k/m/g suffix.
pub(crate) fn parse_stack_size(s: &str) -> Result<usize, crate::error::Error> {
    let t = s.trim();
    let (num, mult): (&str, usize) = match t.as_bytes().last() {
        Some(b'k' | b'K') => (&t[..t.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&t[..t.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    let n = num.trim().parse::<usize>().map_err(|_| {
        // The register scopes `--stack-size` to `run`, so that is the component this names.
        crate::error::Error::usage(
            crate::diag::Component::Run,
            format!("cannot parse stack size `{s}` (e.g. 8m, 1g, 67108864)"),
        )
    })?;
    let bytes = n.saturating_mul(mult);
    // The lower bound protects the engine's own prologue plus margin; the upper bound catches
    // typos (even a virtual reservation should not ask for 128T).
    if !(1 << 20..=1 << 40).contains(&bytes) {
        return Err(crate::error::Error::usage(
            crate::diag::Component::Run,
            format!("stack size {s} is outside the [1m, 1t] valid range"),
        ));
    }
    Ok(bytes)
}

pub(crate) fn run_loaded(
    module: crate::vm::ir::Module,
    instance: crate::vm::instance::Instance,
    vm_call: Option<&str>,
    heat: crate::vm::jit::Heat,
) -> i32 {
    let shared = crate::vm::ctx::Shared::new_loaded(module, instance);
    shared.set_heat(heat);
    // Make entry stubs executable: recipe -> closure -> stub bytes -> whole-region RX (a
    // full-phase step alongside the two above; an occupied region means load failure).
    let engine = match crate::vm::ctx::Engine::try_new(shared) {
        Ok(engine) => engine,
        Err(e) => {
            crate::diag_error!(Engine, "{e}");
            return 70;
        }
    };
    let Some(spec) = vm_call else {
        // main startup chain: interpret lang_start as usual; the exit code is Termination's product.
        let execution = engine.clone();
        let result = match on_guest_stack(move || crate::vm::run_main(&execution)) {
            Ok(result) => result,
            Err(error) => {
                crate::diag_error!(Engine, "{}", error.message);
                engine.exit_process();
                return error.exit_code;
            }
        };
        let code = match result {
            Ok(crate::vm::RunOutcome::Returned(code)) => code,
            // `lang_start` has already run the guest panic hook. Match native
            // stderr here and only translate the structured outcome to its OS
            // exit status.
            Ok(crate::vm::RunOutcome::GuestPanic) => 101,
            Err(e) => {
                crate::diag_error!(Engine, "{e}");
                e.exit_code
            }
        };
        engine.exit_process();
        return code;
    };
    let (name, args) = match parse_vm_call(spec) {
        Ok(v) => v,
        Err(e) => {
            crate::diag_error!(Run, "fail to resolve `--vm-call`: {e}");
            return 2;
        }
    };
    let execution = engine.clone();
    let result = match on_guest_stack(move || {
        // CLI arguments are scalar u64 slots parsed for the explicitly named
        // dev export; pointer-bearing embedding calls are not exposed here.
        unsafe { crate::vm::raw::run_export_raw(&execution, &name, &args) }
    }) {
        Ok(result) => result,
        Err(error) => {
            crate::diag_error!(Engine, "{}", error.message);
            engine.exit_process();
            return error.exit_code;
        }
    };
    let code = match result {
        Ok(crate::vm::RunOutcome::Returned(r)) => {
            crate::out::line(r.lo);
            0
        }
        Ok(crate::vm::RunOutcome::GuestPanic) => {
            crate::diag_error!(Engine, "guest panic not caught");
            101
        }
        Err(e) => {
            crate::diag_error!(Engine, "{e}");
            e.exit_code
        }
    };
    // One dev export is called in a process that ends when it returns, so this is still the
    // process-exit path: the export's own output is out, and the Engine's exit-time work follows it.
    engine.exit_process();
    code
}

/// Guest main execution runs on a dedicated large-stack thread (1 GiB of virtual reservation by
/// default, committed on demand on Linux). An interpreted frame costs the host tens of times more
/// than a native frame; borrowing the caller's 8-16 MiB stack holds only ~8k frames, badly
/// distorting the native stack limit. Guest panics are already absorbed inside run_main/run_export;
/// anything escaping the join is a host panic (a VM bug) and is resumed unchanged, never swallowed.
struct GuestStackStartError {
    message: String,
    exit_code: i32,
}

fn on_guest_stack<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R, GuestStackStartError> {
    let reserve = match crate::options::stack_size() {
        Some(s) => parse_stack_size(&s).map_err(|error| GuestStackStartError {
            message: error.to_string(),
            exit_code: i32::from(error.exit_code()),
        })?,
        None => 1 << 30,
    };
    let spawned = std::thread::Builder::new()
        .name("mirvm-guest".into())
        .stack_size(reserve)
        .spawn(f);
    match spawned {
        Ok(h) => match h.join() {
            Ok(r) => Ok(r),
            Err(host_panic) => std::panic::resume_unwind(host_panic),
        },
        Err(error) => {
            // Do not silently fall back to the caller's small stack (stack semantics would quietly
            // get worse); exit loudly and offer the knob. A typical trigger is a strict commit
            // environment with vm.overcommit_memory=2.
            Err(GuestStackStartError {
                message: format!(
                    "failed to create the guest execution thread ({reserve} bytes of stack \
                     reserved): {error}; retry with a smaller --stack-size / MIRVM_STACK_SIZE"
                ),
                exit_code: 70,
            })
        }
    }
}

/// Parse `name(1,2,…)` (or a bare `name`, which means no arguments).
fn parse_vm_call(spec: &str) -> Result<(String, Vec<u64>), crate::error::Error> {
    let spec = spec.trim();
    let Some(open) = spec.find('(') else {
        return Ok((spec.to_string(), Vec::new()));
    };
    let name = spec[..open].trim().to_string();
    let inner = spec[open + 1..]
        .strip_suffix(')')
        .ok_or(crate::fail!(Run, "missing closing parenthesis"))?;
    let mut args = Vec::new();
    for part in inner.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        args.push(
            p.parse::<u64>()
                .map_err(|e| crate::fail!(Run, format!("argument `{p}`: {e}")))?,
        );
    }
    Ok((name, args))
}
