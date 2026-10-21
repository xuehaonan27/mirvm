//! Isolated preparation of the same canonical std roots used by an in-session base miss.

use std::path::PathBuf;
use std::process::ExitCode;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::diag::Component;
use crate::image::base;

struct BaseBuildCallbacks {
    out: PathBuf,
    ok: bool,
    args: Vec<String>,
}

impl Callbacks for BaseBuildCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        // The base image is built by this session, so its diagnostics belong to the same stream as
        // everything else the command says.
        crate::cli::diagnostics::route_front_end_output(&mut config.opts);
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let (module, instance, exports) = crate::lower::lower_for_base_build(tcx);
        let sess = tcx.sess;
        let fp = (
            sess.ub_checks(),
            sess.overflow_checks(),
            sess.contract_checks(),
        );
        match base::store(&self.out, module, &instance, exports, fp, &self.args) {
            Ok(()) => self.ok = true,
            Err(error) => crate::diag::emit(&crate::error::Error::from(error)),
        }
        Compilation::Stop
    }
}

/// Isolated preparation entry point. argv = [<output path>, optional rustc flags...].
pub(super) fn build_main(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let Some(out) = argv.next() else {
        return crate::error::Error::usage(Component::BaseImage, "missing output path").report();
    };
    let out = PathBuf::from(out);
    // The seed source is staged next to the image it produces, inside the family directory the
    // parent process already created.
    let Some(src_dir) = out.parent().map(|dir| dir.join("src")) else {
        return crate::error::Error::usage(
            Component::BaseImage,
            format!("output path has no parent directory: {}", out.display()),
        )
        .report();
    };
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(path) => path.display().to_string(),
        Err(error) => return crate::error::Error::from(error).report(),
    };
    // An empty crate hosts analysis; lowering seeds only the canonical std roots.
    if let Err(error) = std::fs::create_dir_all(&src_dir) {
        return crate::error::Error::failure(
            Component::BaseImage,
            format!(
                "cannot create the seed directory {}: {error}",
                src_dir.display()
            ),
        )
        .report();
    }
    let src = src_dir.join("empty_main.rs");
    if let Err(error) = std::fs::write(&src, "fn main() {}\n") {
        return crate::error::Error::failure(
            Component::BaseImage,
            format!("cannot write the seed source {}: {error}", src.display()),
        )
        .report();
    }

    let mut rustc_args = vec![
        "mirvm-base-build".to_string(),
        src.display().to_string(),
        "--edition=2024".to_string(),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    rustc_args.extend(crate::cargoless::schedule::GUEST_MIR_ARGS.map(str::to_owned));
    // Thread count must not change the canonical root traversal or the published bytes.
    rustc_args.extend(argv);
    let mut callbacks = BaseBuildCallbacks {
        out,
        ok: false,
        args: rustc_args.clone(),
    };
    rustc_args.push(crate::cli::parallel_frontend_arg().to_string());
    let _compiler_session = crate::cli::compiler_session_guard();
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    if code != ExitCode::SUCCESS || !callbacks.ok {
        return ExitCode::from(crate::diag::exit::FAILURE);
    }
    ExitCode::SUCCESS
}
