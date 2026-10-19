//! Prepared-image dispatch and guest execution shared by both CLI builds.

use super::{
    CAPTURE_DIRECTORY, GuestProcessState, capture_directory, capture_directory_is_forwarded,
    diagnostics,
};
use std::process::{ExitCode, exit};

#[cfg(feature = "compiler")]
mod compiler;
pub(super) use crate::runtime::entry::parse_stack_size;
#[cfg(all(feature = "compiler", test))]
pub(super) use compiler::is_runner_warning_summary;
#[cfg(feature = "compiler")]
pub(crate) use compiler::pack_driver;

#[derive(Default)]
struct PhaseTiming {
    #[cfg(feature = "compiler")]
    base: crate::image::base::Timing,
    frontend: Option<std::time::Duration>,
    lower: Option<std::time::Duration>,
    cache_load: Option<std::time::Duration>,
    cache_store: Option<std::time::Duration>,
    layer_load: Option<std::time::Duration>,
}

/// With `MIRVM_TIMING=1` (or the --vm-stats instrument) prints a one-line phase ledger to
/// stderr. Off by default: stderr takes part in byte-for-byte native differential comparison, so
/// it must not carry noise.
fn print_phase_timing(
    timing: &PhaseTiming,
    engine: Option<std::time::Duration>,
    total: std::time::Duration,
    force: bool,
) {
    if !force && !crate::options::timing() {
        return;
    }
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let mut line = String::from("mirvm-timing:");
    #[cfg(feature = "compiler")]
    if !timing.base.load.is_zero() {
        line.push_str(&format!(" base-load={:.1}ms", ms(timing.base.load)));
    }
    #[cfg(feature = "compiler")]
    if let Some(d) = timing.base.build {
        line.push_str(&format!(" base-build={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.layer_load {
        line.push_str(&format!(" layer-load={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.cache_load {
        line.push_str(&format!(" cache-load={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.frontend {
        line.push_str(&format!(" frontend={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.lower {
        line.push_str(&format!(" lower={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.cache_store {
        line.push_str(&format!(" cache-store={:.1}ms", ms(d)));
    }
    if let Some(d) = engine {
        line.push_str(&format!(" engine={:.1}ms", ms(d)));
    }
    line.push_str(&format!(" total={:.1}ms", ms(total)));
    crate::diag::instrument(format_args!("{line}"));
}

pub(super) fn run_verified_engine(
    artifact: crate::vm::archive::VerifiedArtifact,
    program_argv: &[String],
    vm_call: Option<&str>,
    vm_stats: bool,
    heat: crate::vm::jit::Heat,
) -> i32 {
    let (mut module, mut instance) = artifact.into_parts();
    if crate::options::a2_debug() {
        // A digest over what this session is about to run: the same program lowered cold and one that
        // reused stored layers must produce the same functions, in the same order, with the same
        // absolute ids.
        use crate::vm::ir::frag;
        let mut bodies = crate::store::entry::Key::new();
        let dump = std::env::var_os("MIRVM_MODULE_DUMP").is_some();
        for (index, body) in module.funcs.iter().enumerate() {
            let bytes = frag::encode(body).unwrap_or_default();
            let id = crate::utils::content::digest_hex(&frag::id_of(&bytes));
            if dump {
                crate::diag::instrument(format_args!("[a2-dump] {index} {id} {}", body.name));
            }
            if dump && body.name.contains("Blake2bVarCore8compress") {
                let canonical = frag::canonical(body);
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                crate::diag::instrument(format_args!("[a2-targets] {:?}", canonical.targets));
                crate::diag::instrument(format_args!(
                    "[a2-body] {} blocks {} frame {} targets {} bytes {} {}",
                    body.name,
                    body.blocks.len(),
                    body.frame_size,
                    canonical.targets.len(),
                    bytes.len(),
                    hex
                ));
            }
            bodies.part(&id);
        }
        let mut exports: Vec<(&str, u32)> = module
            .exports
            .iter()
            .map(|(name, id)| (name.as_ref(), *id))
            .collect();
        exports.sort_unstable();
        let mut tables = crate::store::entry::Key::new();
        for (name, id) in &exports {
            tables.part(&format!("{name}={id}"));
        }
        tables.part(&format!(
            "tls={} entries={} relocs={}",
            module.tls.len(),
            module.entry_stub_sites.len(),
            module.frozen_relocs.len()
        ));
        // The frozen bytes are not part of any body's canonical form: a constant whose *content*
        // differed between two sessions would be invisible to the digest above.
        let mut data = crate::store::entry::Key::new();
        if let Some(frozen) = module.frozen.as_ref() {
            data.part(&crate::utils::content::digest_hex(
                &crate::vm::ir::frag::id_of(frozen.bytes()),
            ));
        }
        data.part(&format!("{} arenas", instance.image_frozens.len()));
        for arena in &instance.image_frozens {
            data.part(&format!("{:#x}:{}", arena.home(), arena.used()));
            data.part(&crate::utils::content::digest_hex(
                &crate::vm::ir::frag::id_of(arena.snapshot()),
            ));
        }
        crate::diag::instrument(format_args!(
            "[a2-debug] module: funcs {} bodies {} exports {} tables {} data {}",
            module.funcs.len(),
            bodies.digest(),
            exports.len(),
            tables.digest(),
            data.digest()
        ));
    }
    if vm_stats {
        // One call, and it must stay one call: growing this branch's body changes the codegen of the
        // enclosing function enough to flip the pinned toolchain's release-build miscompile of the
        // interpreter's cleanup chain, which turns a guest panic into SIGABRT instead of the
        // native-matching 101 (`open-issues.md` E35, with the bisect evidence). The report owner
        // renders; this frame only dispatches.
        crate::vm::stats::print(&module);
        return 0;
    }
    // Finalize argv: runtime input is placed after snapshot semantics; one path for cold and warm.
    if let Err(e) = instance.finalize_entry_argv(&mut module, program_argv) {
        crate::diag_error!(Engine, "{e}");
        return 70;
    }
    let mut capture = if let Some(directory) = CAPTURE_DIRECTORY.get() {
        // Name the file after the process generation the header will record, so
        // a parent and its forked children are distinguishable before decoding.
        let process_generation = crate::telemetry::capture::claim_process_generation();
        let output = directory.join(format!(
            "events-{}-{process_generation}.mlog",
            std::process::id()
        ));
        let options = crate::telemetry::CaptureOptions::new(output)
            .with_process_generation(process_generation);
        match crate::telemetry::CaptureSession::start(options) {
            Ok(session) => Some(session),
            Err(error) => {
                crate::diag_error!(Capture, "cannot start event writer: {error}");
                return 70;
            }
        }
    } else {
        None
    };
    let code = crate::runtime::entry::run_loaded(module, instance, vm_call, heat);
    if let Some(session) = &mut capture {
        match session.finish(std::time::Duration::from_secs(30)) {
            Ok(crate::telemetry::CaptureFinish::Finished(_)) => {}
            Ok(crate::telemetry::CaptureFinish::InProgress) => {
                crate::diag_error!(Capture, "writer did not finish within 30 seconds");
                return 70;
            }
            Err(error) => {
                crate::diag_error!(Capture, "cannot finish event file: {error}");
                return 70;
            }
        }
    }
    code
}

pub(crate) fn run_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    dump_mir: bool,
    vm_call: Option<String>,
    vm_stats: bool,
    suppress_runner_warning_summary: bool,
    guest_process: Option<GuestProcessState>,
) -> ExitCode {
    run_driver_inner(
        rustc_args,
        program_argv,
        dump_mir,
        vm_call,
        vm_stats,
        suppress_runner_warning_summary,
        guest_process,
        None,
    )
    .expect("ordinary driver always runs")
}

pub(crate) fn try_prepared_launch(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    validation: std::time::Duration,
) -> Option<ExitCode> {
    run_driver_inner(
        rustc_args,
        program_argv,
        false,
        None,
        false,
        true,
        None,
        Some(validation),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_driver_inner(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    dump_mir: bool,
    vm_call: Option<String>,
    vm_stats: bool,
    suppress_runner_warning_summary: bool,
    guest_process: Option<GuestProcessState>,
    prepared: Option<std::time::Duration>,
) -> Option<ExitCode> {
    if crate::cli::prepare_only() && crate::options::no_prelower() {
        return Some(ExitCode::SUCCESS);
    }
    if let Err(error) = crate::sysroot::pin_args(&rustc_args) {
        crate::diag::emit(&crate::error::Error::from(error));
        return Some(ExitCode::from(crate::diag::exit::FAILURE));
    }
    let mut diagnostic_router = match diagnostics::DiagnosticRouter::start(
        capture_directory(),
        capture_directory_is_forwarded(),
    ) {
        Ok(router) => router,
        Err(error) => {
            crate::diag_error!(Capture, "cannot start diagnostics stream: {error}");
            return Some(ExitCode::from(crate::diag::exit::SOFTWARE));
        }
    };
    let t_start = std::time::Instant::now();
    // Image stack: load the base image plus the dependency image chain (failure or bypass = empty
    // stack, which self-heals onto the full cold path). The lowering fingerprint
    // (ub/overflow/contract checks) can only be verified inside the session; after_analysis does.
    if !dump_mir && let Some(artifact) = crate::image::execution::lookup(&rustc_args) {
        if let Some(validation) = prepared
            && crate::options::timing()
        {
            crate::diag::instrument(format_args!(
                "mirvm-launch: hit validation={:.3}ms",
                validation.as_secs_f64() * 1000.0
            ));
        }
        diagnostic_router.seal_compiler();
        let timing = PhaseTiming {
            cache_load: Some(t_start.elapsed()),
            ..PhaseTiming::default()
        };
        if crate::cli::prepare_only() {
            print_phase_timing(&timing, None, t_start.elapsed(), crate::diag::verbose());
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                return Some(ExitCode::from(crate::diag::exit::SOFTWARE));
            }
            return Some(ExitCode::SUCCESS);
        }
        if let Some(guest) = &guest_process
            && let Err(error) = guest.enter()
        {
            crate::diag::emit(&crate::error::Error::from(error));
            return Some(ExitCode::from(crate::diag::exit::FAILURE));
        }
        let t_engine = std::time::Instant::now();
        let code = run_verified_engine(
            artifact,
            &program_argv,
            vm_call.as_deref(),
            vm_stats,
            crate::image::program::heat(&rustc_args),
        );
        print_phase_timing(
            &timing,
            (!vm_stats).then(|| t_engine.elapsed()),
            t_start.elapsed(),
            vm_stats,
        );
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            exit(crate::diag::exit::SOFTWARE.into());
        }
        exit(code);
    }
    if prepared.is_some() {
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            return Some(ExitCode::from(crate::diag::exit::SOFTWARE));
        }
        return None;
    }
    #[cfg(feature = "compiler")]
    {
        Some(compiler::run_miss(
            rustc_args,
            program_argv,
            dump_mir,
            vm_call,
            vm_stats,
            suppress_runner_warning_summary,
            guest_process,
            diagnostic_router,
            t_start,
        ))
    }
    #[cfg(not(feature = "compiler"))]
    {
        Some(super::worker::run(super::worker::RunRequest {
            rustc_args,
            program_argv,
            dump_mir,
            vm_call,
            vm_stats,
            suppress_runner_warning_summary,
            guest_process,
            pack_out: None,
        }))
    }
}

#[cfg(not(feature = "compiler"))]
pub(crate) fn pack_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    out: std::path::PathBuf,
) -> ExitCode {
    super::worker::run(super::worker::RunRequest {
        rustc_args,
        program_argv,
        dump_mir: false,
        vm_call: None,
        vm_stats: false,
        suppress_runner_warning_summary: true,
        guest_process: None,
        pack_out: Some(out),
    })
}
