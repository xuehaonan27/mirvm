//! One shape for everything mirvm says: component, severity, message.
//!
//! Text rendering is `mirvm[component]: severity: message`; a line with no component in scope is
//! `mirvm: severity: message`. Machine rendering (`MIRVM_OUTPUT=json`) is one JSON object per line,
//! carrying the same fields plus the failure's stable code, its structured details and its cause
//! chain. `docs/current-status.md` owns the frozen surfaces around this vocabulary.
//!
//! Two sinks, and the difference is a correctness constraint rather than a style choice:
//!
//! - [`emit`] (routed) writes the line to inherited fd 2 and hands the same bytes to the capture tee,
//!   so a `mirvm capture` session records MIRVM control diagnostics byte-for-byte. It takes the tee
//!   lock, so it is legal only on ordinary thread context.
//! - [`emit_direct`] writes fd 2 only and takes no lock. Signal-adjacent and teardown paths (the
//!   engine's Drop chain, the syscall trampoline) run where that lock can deadlock, so their lines
//!   are deliberately absent from `diagnostics.log`.
//!
//! Guest fd 1/fd 2 and the rustc emitter never pass through here: the first is the guest's own
//! channel, the second belongs to rustc.
//!
//! This module is compiled source-for-source into the TSan harness, so it must stay pure `std`.

// The vocabulary is one contract, declared whole: a component, severity or failure class that no
// call site constructs yet is a spelling the conversions still to come must not invent locally.
// The allow is deleted in the change that converts the last raw `eprintln!` call site.
#![allow(dead_code)]

pub mod exit;
pub(crate) mod json;
pub(crate) mod table;

use std::fmt::{self, Write as _};
use std::sync::OnceLock;

/// Which subsystem is speaking. The bracket in `mirvm[component]:` is this value's [`name`].
///
/// [`name`]: Component::name
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Component {
    Run,
    Pack,
    Test,
    Capture,
    Cache,
    Log,
    Deps,
    Options,
    Runner,
    BaseImage,
    Doctest,
    Engine,
    Jit,
    Lower,
    Native,
    Resolver,
    Build,
    Store,
    Sysroot,
    Image,
    Syscall,
}

impl Component {
    /// The spelling inside the brackets, and the `component` field of a JSON line.
    pub fn name(self) -> &'static str {
        match self {
            Component::Run => "run",
            Component::Pack => "pack",
            Component::Test => "test",
            Component::Capture => "capture",
            Component::Cache => "cache",
            Component::Log => "log",
            Component::Deps => "deps",
            Component::Options => "options",
            Component::Runner => "runner",
            Component::BaseImage => "base-image",
            Component::Doctest => "doctest",
            Component::Engine => "engine",
            Component::Jit => "jit",
            Component::Lower => "lower",
            Component::Native => "native",
            Component::Resolver => "resolver",
            Component::Build => "build",
            Component::Store => "store",
            Component::Sysroot => "sysroot",
            Component::Image => "image",
            Component::Syscall => "syscall",
        }
    }
}

/// How serious a line is. Every emitted line carries one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Info,
    Debug,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
            Severity::Info => "info",
            Severity::Debug => "debug",
        }
    }
}

/// The exit-code class of a failure. The number itself lives in [`exit`], once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The operation failed: I/O, resolution, compilation, packaging.
    Failure,
    /// The command line, an environment value or an input spelling was rejected.
    Usage,
    /// An internal invariant broke, or a subprocess or thread could not be started.
    Software,
    /// A test run failed (the cargo/rustc convention).
    Test,
}

impl Kind {
    pub fn code(self) -> u8 {
        match self {
            Kind::Failure => exit::FAILURE,
            Kind::Usage => exit::USAGE,
            Kind::Software => exit::SOFTWARE,
            Kind::Test => exit::TEST_FAILED,
        }
    }
}

/// Everything mirvm reports: a typed error, or an event.
///
/// `Display` is the human message; the other methods are what a structured consumer needs. `code`
/// is the stable machine identity declared in the `diag_codes!` register next to the variant it
/// belongs to, and `details` is a pre-rendered JSON object produced by the type that owns the
/// fields — this module splices it verbatim and never inspects it.
pub trait Diagnostic: fmt::Display {
    fn severity(&self) -> Severity;
    /// `None` when no component owns the line (an embedded use with no command scope).
    fn component(&self) -> Option<Component>;
    /// The stable failure identity; `None` for events and for the unregistered tail.
    fn code(&self) -> Option<&'static str> {
        None
    }
    /// The exit-code class. Only meaningful for [`Severity::Error`].
    fn kind(&self) -> Kind {
        Kind::Failure
    }
    /// A pre-rendered JSON object with this failure's structured fields.
    fn details(&self) -> Option<String> {
        None
    }
    /// The cause chain, outermost first, already rendered.
    fn causes(&self) -> Vec<String> {
        Vec::new()
    }
    /// The help block that goes with a rejected argument. Text mode prints it after the message;
    /// machine mode carries it as a field instead, so a consumer never receives a mixed stream.
    fn usage(&self) -> Option<&str> {
        None
    }
}

/// One non-error line: severity, component and an already-formatted message.
pub struct Event {
    severity: Severity,
    component: Option<Component>,
    message: String,
}

impl Event {
    pub fn new(severity: Severity, component: Option<Component>, message: String) -> Self {
        Self {
            severity,
            component,
            message,
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Diagnostic for Event {
    fn severity(&self) -> Severity {
        self.severity
    }

    fn component(&self) -> Option<Component> {
        self.component
    }
}

static SCOPE: OnceLock<Component> = OnceLock::new();

/// Declare which component this process is speaking for. Called once at a command boundary; the
/// first caller wins, so an outer command (`capture`) keeps the scope of an inner one (`run`).
pub fn enter(component: Component) {
    let _ = SCOPE.set(component);
}

/// The component a line belongs to when its call site does not name one.
pub fn current() -> Option<Component> {
    SCOPE.get().copied()
}

static TEE: OnceLock<fn(&[u8])> = OnceLock::new();

/// Install the capture tee. The CLI installs the router's appender; a process with no capture
/// leaves it unset and every line goes to fd 2 alone.
pub fn install_tee(tee: fn(&[u8])) {
    let _ = TEE.set(tee);
}

/// Write these exact bytes to inherited fd 2 and, when a capture owns a tee, the same bytes to it.
///
/// No newline is added: the caller owns the line's bytes, which is what keeps the tee a byte-exact
/// copy of stderr. A failed write is fatal, exactly as an unwritable fd 2 has always been.
pub fn write(bytes: &[u8]) {
    use std::io::Write as _;
    std::io::stderr()
        .write_all(bytes)
        .expect("failed printing to stderr");
    if let Some(tee) = TEE.get() {
        tee(bytes);
    }
}

/// Report one diagnostic: fd 2 plus the capture tee.
pub fn emit(diagnostic: &dyn Diagnostic) {
    let line = render(diagnostic);
    write(line.as_bytes());
    write_usage(diagnostic);
}

/// The help block that belongs to a failure. Machine mode already carries it as a field, so writing
/// it here as well would put prose in the middle of a JSONL stream.
fn write_usage(diagnostic: &dyn Diagnostic) {
    if json::enabled() {
        return;
    }
    if let Some(usage) = diagnostic.usage() {
        write(usage.as_bytes());
    }
}

/// Report one diagnostic to fd 2 alone, taking no lock. See the module note for why some paths
/// cannot use [`emit`].
pub fn emit_direct(diagnostic: &dyn Diagnostic) {
    use std::io::Write as _;
    let line = render(diagnostic);
    let _ = std::io::stderr().write_all(line.as_bytes());
    if !json::enabled()
        && let Some(usage) = diagnostic.usage()
    {
        let _ = std::io::stderr().write_all(usage.as_bytes());
    }
}

/// Render one diagnostic exactly as [`emit`] would, without writing it.
///
/// For the one command that owns its output writers (`mirvm log` takes them so its report can be
/// asserted in a unit test): the rendering, the mode and the grammar still come from here, so a
/// diagnostic cannot look different because of where it is written.
pub fn render_line(diagnostic: &dyn Diagnostic) -> String {
    render(diagnostic)
}

/// Render one diagnostic in the mode `MIRVM_OUTPUT` selects.
fn render(diagnostic: &dyn Diagnostic) -> String {
    if json::enabled() {
        json::line(diagnostic)
    } else {
        text(diagnostic)
    }
}

/// The text rendering, for tests in other modules that pin a failure's line. Reading the mode from
/// the environment would make such a test depend on `MIRVM_OUTPUT`, so this bypasses it.
#[cfg(test)]
pub(crate) fn render_for_test(diagnostic: &dyn Diagnostic) -> String {
    text(diagnostic)
}

/// `mirvm[component]: severity: message`, or `mirvm: severity: message` with no component.
fn text(diagnostic: &dyn Diagnostic) -> String {
    let mut line = String::with_capacity(128);
    line.push_str("mirvm");
    if let Some(component) = diagnostic.component() {
        line.push('[');
        line.push_str(component.name());
        line.push(']');
    }
    line.push_str(": ");
    line.push_str(diagnostic.severity().name());
    line.push_str(": ");
    let _ = write!(line, "{diagnostic}");
    line.push('\n');
    line
}

/// Report at one severity. `component` names the speaker, or the command scope is used.
#[macro_export]
macro_rules! diag_at {
    ($severity:expr, $component:ident, $($arg:tt)*) => {
        $crate::diag::emit(&$crate::diag::Event::new(
            $severity,
            ::core::option::Option::Some($crate::diag::Component::$component),
            ::std::format!($($arg)*),
        ))
    };
    ($severity:expr, $($arg:tt)*) => {
        $crate::diag::emit(&$crate::diag::Event::new(
            $severity,
            $crate::diag::current(),
            ::std::format!($($arg)*),
        ))
    };
}

/// Report at one severity to fd 2 alone, taking no lock.
#[macro_export]
macro_rules! diag_direct_at {
    ($severity:expr, $component:ident, $($arg:tt)*) => {
        $crate::diag::emit_direct(&$crate::diag::Event::new(
            $severity,
            ::core::option::Option::Some($crate::diag::Component::$component),
            ::std::format!($($arg)*),
        ))
    };
    ($severity:expr, $($arg:tt)*) => {
        $crate::diag::emit_direct(&$crate::diag::Event::new(
            $severity,
            $crate::diag::current(),
            ::std::format!($($arg)*),
        ))
    };
}

#[macro_export]
macro_rules! diag_error {
    ($($arg:tt)*) => { $crate::diag_at!($crate::diag::Severity::Error, $($arg)*) };
}

#[macro_export]
macro_rules! diag_warn {
    ($($arg:tt)*) => { $crate::diag_at!($crate::diag::Severity::Warning, $($arg)*) };
}

#[macro_export]
macro_rules! diag_note {
    ($($arg:tt)*) => { $crate::diag_at!($crate::diag::Severity::Note, $($arg)*) };
}

#[macro_export]
macro_rules! diag_info {
    ($($arg:tt)*) => { $crate::diag_at!($crate::diag::Severity::Info, $($arg)*) };
}

#[macro_export]
macro_rules! diag_debug {
    ($($arg:tt)*) => { $crate::diag_at!($crate::diag::Severity::Debug, $($arg)*) };
}

/// Report at one severity to fd 2 alone, taking no lock.
#[macro_export]
macro_rules! diag_direct {
    ($($arg:tt)*) => { $crate::diag_direct_at!($crate::diag::Severity::Debug, $($arg)*) };
}

/// Declare the machine identity, component and exit-code class of every variant of one error enum.
///
/// The register is the single spelling of an error's code: the gate in `tests/lib/modes/repo-quality.sh`
/// fails when a registered code appears anywhere else under `src/`. A variant with no class is
/// `Kind::Failure`.
///
/// The type must derive `thiserror::Error` (for `Display` and the source chain) and
/// `serde::Serialize` (for `details`); `#[serde(skip)]` belongs on `#[source]` fields, which are
/// reported through `causes` instead.
#[macro_export]
macro_rules! diag_codes {
    ($ty:ty: $component:ident => { $( $variant:ident => $code:literal $($kind:ident)? ),* $(,)? }) => {
        impl $crate::diag::Diagnostic for $ty {
            fn severity(&self) -> $crate::diag::Severity {
                $crate::diag::Severity::Error
            }

            fn component(&self) -> ::core::option::Option<$crate::diag::Component> {
                ::core::option::Option::Some($crate::diag::Component::$component)
            }

            fn code(&self) -> ::core::option::Option<&'static str> {
                match self {
                    $( Self::$variant { .. } => ::core::option::Option::Some($code), )*
                }
            }

            fn kind(&self) -> $crate::diag::Kind {
                match self {
                    $( Self::$variant { .. } => $crate::diag_codes!(@kind $($kind)?), )*
                }
            }

            fn details(&self) -> ::core::option::Option<::std::string::String> {
                ::serde_json::to_string(self).ok()
            }

            fn causes(&self) -> ::std::vec::Vec<::std::string::String> {
                let mut out = ::std::vec::Vec::new();
                let mut next: ::core::option::Option<&(dyn ::std::error::Error + 'static)> =
                    ::std::error::Error::source(self);
                while let ::core::option::Option::Some(cause) = next {
                    out.push(cause.to_string());
                    next = cause.source();
                }
                out
            }
        }
    };
    (@kind) => { $crate::diag::Kind::Failure };
    (@kind $kind:ident) => { $crate::diag::Kind::$kind };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line, one shape: the component bracket is present exactly when a component owns the
    /// line, and the severity word is always present.
    #[test]
    fn text_grammar_is_stable() {
        for (severity, name) in [
            (Severity::Error, "error"),
            (Severity::Warning, "warning"),
            (Severity::Note, "note"),
            (Severity::Info, "info"),
            (Severity::Debug, "debug"),
        ] {
            let scoped = Event::new(severity, Some(Component::Engine), "boom".into());
            assert_eq!(text(&scoped), format!("mirvm[engine]: {name}: boom\n"));
            let bare = Event::new(severity, current(), "boom".into());
            assert_eq!(text(&bare), format!("mirvm: {name}: boom\n"));
        }
        assert_eq!(
            Component::BaseImage.name(),
            "base-image",
            "a component's bracket spelling is what a consumer matches on"
        );
    }

    /// The machine envelope: versioned, one object per line, with the failure's identity, its
    /// structured details and its cause chain as separate fields.
    #[test]
    fn json_envelope_is_versioned_and_field_separated() {
        let event = Event::new(
            Severity::Error,
            Some(Component::Run),
            "a \"quoted\" line".into(),
        );
        assert_eq!(
            json::line(&event),
            "{\"v\":1,\"severity\":\"error\",\"component\":\"run\",\"code\":null,\
             \"message\":\"a \\\"quoted\\\" line\",\"details\":null,\"causes\":[],\
             \"usage\":null,\"exit_code\":1}\n"
        );
        // An event reports no exit code: the process is not about to exit with the class of a line
        // it merely printed.
        let info = Event::new(Severity::Info, None, "hi".into());
        let line = json::line(&info);
        assert!(!line.contains("exit_code"), "{line}");
        assert!(line.starts_with("{\"v\":1,\"severity\":\"info\",\"component\":null,"));
    }
}
