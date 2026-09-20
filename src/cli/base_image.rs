//! The `mirvm __build-base-image <path>` subprocess.
//!
//! The base image is built by a compiler session of its own, because one process cannot start a
//! second rustc session (see `crate::image::base`). This module is the driver only: it stages the
//! synthetic seed source, runs the session, and hands the lowering product to
//! [`crate::image::base::store`], which owns the file format and the publishability rules.

use std::path::PathBuf;
use std::process::ExitCode;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::image::base;

struct BaseBuildCallbacks {
    out: PathBuf,
    ok: bool,
}

impl Callbacks for BaseBuildCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let (module, exports) = crate::lower::lower_for_base_build(tcx);
        let sess = tcx.sess;
        let fp = (
            sess.ub_checks(),
            sess.overflow_checks(),
            sess.contract_checks(),
        );
        match base::store(&self.out, module, exports, fp) {
            Ok(()) => self.ok = true,
            Err(reason) => eprintln!("base-image: {reason}, giving up"),
        }
        Compilation::Stop
    }
}

/// Subprocess entry point. argv = [<output path>].
pub(super) fn build_main(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let Some(out) = argv.next() else {
        eprintln!("__build-base-image: missing output path");
        return ExitCode::from(2);
    };
    let out = PathBuf::from(out);
    // The seed source is staged next to the image it produces, inside the family directory the
    // parent process already created.
    let Some(src_dir) = out.parent().map(|dir| dir.join("src")) else {
        eprintln!("__build-base-image: output path has no parent directory");
        return ExitCode::from(2);
    };
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(path) => path.display().to_string(),
        Err(error) => {
            eprintln!("__build-base-image: sysroot unavailable: {error}");
            return ExitCode::from(1);
        }
    };
    // Synthetic empty main: the deterministic base-image seed
    if std::fs::create_dir_all(&src_dir).is_err() {
        return ExitCode::from(1);
    }
    let src = src_dir.join("empty_main.rs");
    if std::fs::write(&src, "fn main() {}\n").is_err() {
        return ExitCode::from(1);
    }

    let mut rustc_args = vec![
        "mirvm-base-build".to_string(),
        src.display().to_string(),
        "--edition=2024".to_string(),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    // The base image key is digest(build_id, sysroot stamp) and never sees these arguments, so the
    // frontend flag can go straight in. Applying it here matters as much as in the runner: a
    // parallel frontend that reordered emitted functions would change the image bytes while the
    // key stayed the same.
    rustc_args.push(crate::cli::parallel_frontend_arg().to_string());
    let mut callbacks = BaseBuildCallbacks { out, ok: false };
    let _compiler_session = crate::cli::compiler_session_guard();
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    if code != ExitCode::SUCCESS || !callbacks.ok {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
