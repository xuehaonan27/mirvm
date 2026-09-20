//! Cold-path routing for compiler and MIRVM control diagnostics.
//!
//! Guest fd 2 is deliberately outside this module. It continues to write to
//! the inherited process descriptor through the normal guest/host paths.

use std::env;
use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rustc_errors::annotate_snippet_emitter_writer::AnnotateSnippetEmitter;
use rustc_errors::emitter::{
    Destination, DynEmitter, HumanReadableErrorType, OutputTheme, get_stderr_color_choice,
};
use rustc_errors::json::JsonEmitter;
use rustc_errors::{AutoStream, TerminalUrl};
use rustc_session::config::{ErrorOutputType, Options};
use rustc_session::parse::ParseSess;

const FINAL_NAME: &str = "diagnostics.log";
const PARTIAL_NAME: &str = "diagnostics.log.partial";
const ATTACHED_NAME: &str = "diagnostics.log.attached";

static ACTIVE: Mutex<Option<RouterState>> = Mutex::new(None);
static ATEXIT_RESULT: OnceLock<i32> = OnceLock::new();

#[derive(Clone, Copy)]
enum DiagnosticSource {
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

/// Emit one MIRVM-owned control diagnostic to inherited stderr and, when a
/// capture owns a router, append the same bytes to its diagnostics stream.
pub(crate) fn control(arguments: fmt::Arguments<'_>) {
    let mut bytes = Vec::new();
    bytes
        .write_fmt(arguments)
        .expect("formatting into a byte vector cannot fail");
    bytes.push(b'\n');

    crate::diag::write(&bytes);
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

/// A copy of the pinned rustc diagnostic options needed to replace its
/// emitter only when a capture is active.
pub(crate) struct CompilerEmitterSpec {
    options: Options,
}

impl CompilerEmitterSpec {
    pub(crate) fn for_capture(options: &Options, active: bool) -> Option<Self> {
        active.then(|| Self {
            options: options.clone(),
        })
    }

    pub(crate) fn install(self, psess: &mut ParseSess) {
        psess
            .dcx()
            .set_emitter(make_tee_emitter(&self.options, psess));
    }
}

fn resolved_terminal_url(options: &Options) -> TerminalUrl {
    match options.unstable_opts.terminal_urls {
        TerminalUrl::Auto => match (
            env::var("COLORTERM").as_deref(),
            env::var("TERM").as_deref(),
        ) {
            (Ok("truecolor"), Ok("xterm-256color"))
                if options.unstable_features.is_nightly_build() =>
            {
                TerminalUrl::Yes
            }
            _ => TerminalUrl::No,
        },
        value => value,
    }
}

fn make_tee_emitter(options: &Options, psess: &ParseSess) -> Box<DynEmitter> {
    let terminal_url = resolved_terminal_url(options);
    let source_map = if options.unstable_opts.link_only {
        None
    } else {
        Some(psess.clone_source_map())
    };

    match options.error_format {
        ErrorOutputType::HumanReadable { kind, color_config } => {
            let HumanReadableErrorType { short, unicode } = kind;
            let stderr = io::stderr();
            let color_choice = get_stderr_color_choice(color_config, &stderr);
            let writer: Box<dyn Write + Send> = Box::new(BufferedStderrTee::new());
            let destination: Destination = AutoStream::new(writer, color_choice);

            Box::new(
                AnnotateSnippetEmitter::new(destination)
                    .sm(source_map)
                    .short_message(short)
                    .diagnostic_width(options.diagnostic_width)
                    .macro_backtrace(options.unstable_opts.macro_backtrace)
                    .track_diagnostics(options.unstable_opts.track_diagnostics)
                    .terminal_url(terminal_url)
                    .theme(if unicode {
                        OutputTheme::Unicode
                    } else {
                        OutputTheme::Ascii
                    })
                    .ignored_directories_in_source_blocks(
                        options
                            .unstable_opts
                            .ignore_directory_in_diagnostics_source_blocks
                            .clone(),
                    )
                    .ui_testing(options.unstable_opts.ui_testing),
            )
        }
        ErrorOutputType::Json {
            pretty,
            json_rendered,
            color_config,
        } => Box::new(
            JsonEmitter::new(
                Box::new(io::BufWriter::new(BufferedStderrTee::new())),
                source_map,
                pretty,
                json_rendered,
                color_config,
            )
            .ui_testing(options.unstable_opts.ui_testing)
            .ignored_directories_in_source_blocks(
                options
                    .unstable_opts
                    .ignore_directory_in_diagnostics_source_blocks
                    .clone(),
            )
            .diagnostic_width(options.diagnostic_width)
            .macro_backtrace(options.unstable_opts.macro_backtrace)
            .track_diagnostics(options.unstable_opts.track_diagnostics)
            .terminal_url(terminal_url),
        ),
    }
}

/// Mirrors rustc's non-Windows `Buffy`: a renderer can make many small writes,
/// but one diagnostic reaches inherited stderr and the capture sink together
/// at its flush boundary.
struct BufferedStderrTee {
    stderr: io::Stderr,
    buffer: Vec<u8>,
}

impl BufferedStderrTee {
    fn new() -> Self {
        Self {
            stderr: io::stderr(),
            buffer: Vec::new(),
        }
    }
}

impl Write for BufferedStderrTee {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.write_all(&self.buffer)?;
        append(DiagnosticSource::Compiler, &self.buffer);
        self.buffer.clear();
        Ok(())
    }
}

impl Drop for BufferedStderrTee {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            // Do not turn an otherwise normal rustc teardown into a new panic.
            // Finalization normally flushes explicitly; this is only the
            // best-effort remainder path.
            let bytes = std::mem::take(&mut self.buffer);
            let _ = self.stderr.write_all(&bytes);
            append(DiagnosticSource::Compiler, &bytes);
        }
    }
}

fn publish_without_replace(partial_path: &Path, final_path: &Path) -> io::Result<()> {
    let partial = CString::new(partial_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial diagnostics path contains a NUL byte",
        )
    })?;
    let final_path = CString::new(final_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "final diagnostics path contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            partial.as_ptr(),
            libc::AT_FDCWD,
            final_path.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
