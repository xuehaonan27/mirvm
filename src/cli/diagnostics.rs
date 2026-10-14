//! Cold-path routing for compiler and MIRVM control diagnostics.
//!
//! Guest fd 2 is deliberately outside this module. It continues to write to
//! the inherited process descriptor through the normal guest/host paths.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const FINAL_NAME: &str = "diagnostics.log";
const PARTIAL_NAME: &str = "diagnostics.log.partial";
const ATTACHED_NAME: &str = "diagnostics.log.attached";

static ACTIVE: Mutex<Option<RouterState>> = Mutex::new(None);
static ATEXIT_RESULT: OnceLock<i32> = OnceLock::new();

#[derive(Clone, Copy)]
enum DiagnosticSource {
    #[cfg_attr(
        not(feature = "compiler"),
        expect(
            dead_code,
            reason = "Only rustc emitters create compiler records in the shared capture sink."
        )
    )]
    Compiler,
    Control,
}

struct SinkFailure {
    kind: io::ErrorKind,
    message: String,
}

impl SinkFailure {
    fn into_error(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}

struct RouterState {
    file: File,
    partial_path: PathBuf,
    final_path: PathBuf,
    attached_path: PathBuf,
    owns_attachment_marker: bool,
    compiler_sealed: bool,
    failure: Option<SinkFailure>,
}

impl RouterState {
    fn record_failure(&mut self, error: io::Error) {
        if self.failure.is_none() {
            self.failure = Some(SinkFailure {
                kind: error.kind(),
                message: error.to_string(),
            });
        }
    }

    fn append(&mut self, source: DiagnosticSource, bytes: &[u8]) {
        if matches!(source, DiagnosticSource::Compiler) && self.compiler_sealed {
            self.record_failure(io::Error::new(
                io::ErrorKind::InvalidData,
                "compiler diagnostics arrived after the compiler stream was sealed",
            ));
            return;
        }
        if self.failure.is_some() {
            return;
        }
        if let Err(error) = self.file.write_all(bytes) {
            self.record_failure(error);
        }
    }
}

/// Handle for the diagnostics stream of one capture command.
///
/// The command boundary arms the process-global state before argument parsing;
/// later runner and `run_driver` handles join it. This keeps rustc's writer and
/// control boundaries ordered without redirecting process fd 2.
pub(crate) struct DiagnosticRouter {
    active: bool,
    finished: bool,
}

impl DiagnosticRouter {
    pub(crate) fn start(directory: Option<&Path>, allow_attach: bool) -> io::Result<Self> {
        // The emitter routes through this module's appender from here on; installing it is
        // idempotent, and with no router active the appender is a no-op.
        crate::diag::install_tee(tee_control);
        let Some(directory) = directory else {
            return Ok(Self {
                active: false,
                finished: false,
            });
        };

        let final_path = directory.join(FINAL_NAME);
        let partial_path = directory.join(PARTIAL_NAME);
        let attached_path = directory.join(ATTACHED_NAME);
        {
            let active = ACTIVE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = active.as_ref() {
                if state.final_path == final_path {
                    return Ok(Self {
                        active: true,
                        finished: false,
                    });
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a diagnostics router for another session is already active",
                ));
            }
        }
        match std::fs::symlink_metadata(&final_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the final diagnostics file already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let (file, owns_attachment_marker) = if allow_attach {
            match OpenOptions::new().append(true).open(&partial_path) {
                Ok(file) => {
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&attached_path)?;
                    (file, true)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    OpenOptions::new()
                        .append(true)
                        .create_new(true)
                        .open(&partial_path)?,
                    false,
                ),
                Err(error) => return Err(error),
            }
        } else {
            (
                OpenOptions::new()
                    .append(true)
                    .create_new(true)
                    .open(&partial_path)?,
                false,
            )
        };
        register_atexit()?;

        let mut active = ACTIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            drop(active);
            let _ = std::fs::remove_file(&partial_path);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a diagnostics router is already active",
            ));
        }
        *active = Some(RouterState {
            file,
            partial_path,
            final_path,
            attached_path,
            owns_attachment_marker,
            compiler_sealed: false,
            failure: None,
        });
        Ok(Self {
            active: true,
            finished: false,
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active && !self.finished
    }

    /// Must be called only after `rustc_driver::run_compiler` has returned.
    pub(crate) fn seal_compiler(&mut self) {
        if !self.is_active() {
            return;
        }
        if let Some(state) = ACTIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            state.compiler_sealed = true;
        }
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if !self.is_active() {
            self.finished = true;
            return Ok(());
        }
        self.finished = true;
        finish_active()
    }
}

fn register_atexit() -> io::Result<()> {
    let result =
        *ATEXIT_RESULT.get_or_init(|| crate::os::process::atexit_native(finish_at_process_exit));
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::other("cannot register diagnostics finalizer"))
    }
}

extern "C" fn finish_at_process_exit() {
    let _ = finish_active();
}

fn finish_active() -> io::Result<()> {
    let Some(mut state) = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return Ok(());
    };

    if state.failure.is_none()
        && let Err(error) = state.file.flush()
    {
        state.record_failure(error);
    }
    if let Some(failure) = state.failure {
        return Err(failure.into_error());
    }
    drop(state.file);

    let final_present = path_present(&state.final_path)?;
    let partial_present = path_present(&state.partial_path)?;
    if final_present && !partial_present {
        let _ = std::fs::remove_file(&state.attached_path);
        return Ok(());
    }
    if !state.owns_attachment_marker && path_present(&state.attached_path)? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "a forwarded diagnostics writer did not publish its stream",
        ));
    }
    publish_without_replace(&state.partial_path, &state.final_path)?;
    if state.owns_attachment_marker {
        std::fs::remove_file(&state.attached_path)?;
    }
    Ok(())
}

fn path_present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The capture tee `diag` calls for every routed line: the byte-level entry the router owns, so the
/// emitter itself never has to know a capture exists.
fn tee_control(bytes: &[u8]) {
    append(DiagnosticSource::Control, bytes);
}

fn append(source: DiagnosticSource, bytes: &[u8]) {
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = active.as_mut() {
        state.append(source, bytes);
    }
}

fn publish_without_replace(partial_path: &Path, final_path: &Path) -> io::Result<()> {
    let partial =
        CString::new(crate::os::fs::raw_bytes(partial_path.as_os_str())).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "partial diagnostics path contains a NUL byte",
            )
        })?;
    let final_path =
        CString::new(crate::os::fs::raw_bytes(final_path.as_os_str())).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "final diagnostics path contains a NUL byte",
            )
        })?;
    crate::os::fs::rename_noreplace(&partial, &final_path)
}

#[cfg(feature = "compiler")]
mod compiler;
#[cfg(feature = "compiler")]
pub(crate) use compiler::{CompilerEmitterSpec, route_front_end_output};

/// Release this process's attachment marker before exec. The replacement appends to the same
/// partial stream and becomes its finalizer. On exec failure this process can still publish it.
#[cfg(not(feature = "compiler"))]
pub(super) fn prepare_exec() -> io::Result<()> {
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = active.as_mut() {
        if let Some(failure) = state.failure.as_ref() {
            return Err(io::Error::new(failure.kind, failure.message.clone()));
        }
        state.file.flush()?;
        if state.owns_attachment_marker {
            std::fs::remove_file(&state.attached_path)?;
            state.owns_attachment_marker = false;
        }
    }
    Ok(())
}
