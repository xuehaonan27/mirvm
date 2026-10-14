//! rustc emitters for captured or held compiler diagnostics.

use super::{DiagnosticSource, append};
use rustc_errors::annotate_snippet_emitter_writer::AnnotateSnippetEmitter;
use rustc_errors::emitter::{
    ColorConfig, Destination, DynEmitter, HumanReadableErrorType, OutputTheme,
    get_stderr_color_choice,
};
use rustc_errors::json::JsonEmitter;
use rustc_errors::{AutoStream, TerminalUrl};
use rustc_session::config::{ErrorOutputType, Options};
use rustc_session::parse::ParseSess;
use std::env;
use std::io::{self, Write};

/// A copy of the pinned rustc diagnostic options needed to replace its emitter whenever mirvm needs
/// to see the compiler's bytes before the user does: to tee them into a capture stream, to hold them
/// back for a build's outcome, or both.
pub(crate) struct CompilerEmitterSpec {
    options: Options,
    hold: bool,
}

impl CompilerEmitterSpec {
    /// `capture` = the session belongs to a capture and its diagnostics enter that stream. `hold` =
    /// the session's diagnostics wait for [`crate::cli::build_log::finish`] instead of reaching
    /// stderr at once. Neither is wanted, and rustc keeps its own emitter.
    pub(crate) fn for_session(options: &Options, capture: bool, hold: bool) -> Option<Self> {
        (capture || hold).then(|| Self {
            options: options.clone(),
            hold,
        })
    }

    pub(crate) fn install(self, psess: &mut ParseSess) {
        psess
            .dcx()
            .set_emitter(make_tee_emitter(&self.options, psess, self.hold));
    }
}

/// Put the front end into the caller's output mode, before the session is built from `options`.
///
/// `MIRVM_OUTPUT=json` promises one JSON object per line, and the front end's diagnostics are
/// rendered by rustc rather than by [`crate::diag`], so without this its human rendering is the one
/// prose line inside an otherwise machine-readable stream. Text mode leaves the session's own choice
/// alone, which is what keeps the frozen rustc text surface byte-identical. Colors are off in both
/// fields because a machine consumer reads bytes, and `json_rendered` only names the human rendering
/// a consumer would ask for instead, which nothing does in machine mode.
pub(crate) fn route_front_end_output(options: &mut Options) {
    if !crate::options::machine_output() {
        return;
    }
    options.error_format = ErrorOutputType::Json {
        pretty: false,
        json_rendered: HumanReadableErrorType {
            short: false,
            unicode: false,
        },
        color_config: ColorConfig::Never,
    };
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

fn make_tee_emitter(options: &Options, psess: &ParseSess, hold: bool) -> Box<DynEmitter> {
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
            let writer: Box<dyn Write + Send> = Box::new(BufferedStderrTee::new(hold));
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
                Box::new(io::BufWriter::new(BufferedStderrTee::new(hold))),
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
/// but one diagnostic reaches its destination whole, at its flush boundary. The destination is
/// inherited stderr plus the capture sink, or — when `hold` is set — the build log, which decides
/// later whether either one ever sees it.
struct BufferedStderrTee {
    stderr: io::Stderr,
    buffer: Vec<u8>,
    hold: bool,
}

impl BufferedStderrTee {
    fn new(hold: bool) -> Self {
        Self {
            stderr: io::stderr(),
            buffer: Vec::new(),
            hold,
        }
    }

    /// Send one complete diagnostic to wherever it belongs.
    fn release(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.hold {
            crate::cli::build_log::hold(bytes);
            return Ok(());
        }
        self.stderr.write_all(bytes)?;
        append(DiagnosticSource::Compiler, bytes);
        Ok(())
    }
}

impl Write for BufferedStderrTee {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let bytes = std::mem::take(&mut self.buffer);
        self.release(&bytes)
    }
}

impl Drop for BufferedStderrTee {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            // Do not turn an otherwise normal rustc teardown into a new panic.
            // Finalization normally flushes explicitly; this is only the
            // best-effort remainder path.
            let bytes = std::mem::take(&mut self.buffer);
            let _ = self.release(&bytes);
        }
    }
}
