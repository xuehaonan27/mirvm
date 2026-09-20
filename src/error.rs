//! The product's failure root: one type every entry point can return.
//!
//! Each module owns the failures it understands and declares them as a `thiserror` enum registered
//! with `diag_codes!`; [`Error::Owned`] carries one of those, so `?` works across the whole tree and
//! the root never has to learn a new variant — a module error joins it with a one-line `From`. A
//! failure mirvm authors in place (a rejected argument, a missing input, a broken invariant) needs
//! no enum of its own yet, so [`Error::Plain`] carries the component, the class and the message
//! directly.
//!
//! Errors are emitted exactly once. The rule is placement, not state: whoever owns the diagnostics
//! router emits before releasing it, so a capture session records the failure byte-for-byte.
//! [`Error::report`] is that emission plus the exit code, and it is the only place that turns a
//! failure into a process status.

use std::process::ExitCode;

use crate::diag::{self, Component, Diagnostic, Kind, Severity};

/// A module error as the root sees it: its own `Display` and its own registered identity.
pub trait Owned: std::error::Error + Diagnostic + Send + Sync + 'static {}

impl<T: std::error::Error + Diagnostic + Send + Sync + 'static> Owned for T {}

/// A mirvm failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A failure authored where it was detected. `usage` is the command's help block, printed after
    /// the message when an argument was rejected.
    #[error("{message}")]
    Plain {
        component: Option<Component>,
        kind: Kind,
        message: String,
        usage: Option<String>,
    },

    /// A failure owned by a module error enum, which carries its own component, code, details and
    /// cause chain.
    #[error("{0}")]
    Owned(Box<dyn Owned>),
}

impl Error {
    /// A rejected command line, environment value or input spelling.
    pub fn usage(component: Component, message: impl Into<String>) -> Self {
        Error::Plain {
            component: Some(component),
            kind: Kind::Usage,
            message: message.into(),
            usage: None,
        }
    }

    /// [`Error::usage`] with the command's help block.
    pub fn usage_with(component: Component, message: impl Into<String>, usage: String) -> Self {
        Error::Plain {
            component: Some(component),
            kind: Kind::Usage,
            message: message.into(),
            usage: Some(usage),
        }
    }

    /// A usage failure raised before any command scope exists, so no component is claimed.
    pub fn usage_unscoped(message: impl Into<String>) -> Self {
        Error::Plain {
            component: None,
            kind: Kind::Usage,
            message: message.into(),
            usage: None,
        }
    }

    /// The operation failed: I/O, resolution, compilation, packaging.
    pub fn failure(component: Component, message: impl Into<String>) -> Self {
        Error::Plain {
            component: Some(component),
            kind: Kind::Failure,
            message: message.into(),
            usage: None,
        }
    }

    /// An internal invariant broke, or a subprocess or thread could not be started.
    pub fn software(component: Component, message: impl Into<String>) -> Self {
        Error::Plain {
            component: Some(component),
            kind: Kind::Software,
            message: message.into(),
            usage: None,
        }
    }

    /// The exit status this failure asks for. A code mirvm did not choose (rustc's, the guest's)
    /// never travels as an `Error`, so every variant here has one of the four named classes.
    pub fn exit_code(&self) -> u8 {
        self.kind().code()
    }

    /// Emit this failure once and turn it into the process status.
    pub fn report(&self) -> ExitCode {
        diag::emit(self);
        ExitCode::from(self.exit_code())
    }

    /// Emit this failure once and end the process. For the deep sites that cannot yet propagate
    /// (`-> !` helpers inside a scheduling or compilation phase).
    pub fn report_and_exit(&self) -> ! {
        diag::emit(self);
        std::process::exit(self.exit_code().into())
    }
}

impl Diagnostic for Error {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn component(&self) -> Option<Component> {
        match self {
            Error::Plain { component, .. } => *component,
            Error::Owned(owned) => owned.component(),
        }
    }

    /// The class-derived code of an in-place failure: the command-line layer detected those itself,
    /// and each is replaced by a specific variant as its owner is typed.
    fn code(&self) -> Option<&'static str> {
        match self {
            Error::Plain { kind, .. } => Some(match kind {
                Kind::Usage => "cli.usage",
                Kind::Failure => "cli.failure",
                Kind::Software => "cli.software",
                Kind::Test => "cli.test",
            }),
            Error::Owned(owned) => owned.code(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Error::Plain { kind, .. } => *kind,
            Error::Owned(owned) => owned.kind(),
        }
    }

    fn details(&self) -> Option<String> {
        match self {
            Error::Plain { .. } => None,
            Error::Owned(owned) => owned.details(),
        }
    }

    fn causes(&self) -> Vec<String> {
        match self {
            Error::Plain { .. } => Vec::new(),
            Error::Owned(owned) => owned.causes(),
        }
    }

    fn usage(&self) -> Option<&str> {
        match self {
            Error::Plain { usage, .. } => usage.as_deref(),
            Error::Owned(owned) => owned.usage(),
        }
    }
}

impl From<crate::sysroot::Error> for Error {
    fn from(error: crate::sysroot::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

impl From<crate::options::Error> for Error {
    fn from(error: crate::options::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

impl From<crate::pack::Error> for Error {
    fn from(error: crate::pack::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

impl From<crate::image::Error> for Error {
    fn from(error: crate::image::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

impl From<crate::cli::Error> for Error {
    fn from(error: crate::cli::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

impl From<crate::cargo_shim::Error> for Error {
    fn from(error: crate::cargo_shim::Error) -> Self {
        Error::Owned(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::exit;

    #[test]
    fn plain_failure_renders_one_line_and_asks_for_its_class() {
        let error = Error::usage_with(
            Component::Pack,
            "unknown argument `--x`",
            "usage: mirvm pack <target>\n".to_string(),
        );
        assert_eq!(error.to_string(), "unknown argument `--x`");
        assert_eq!(error.component(), Some(Component::Pack));
        assert_eq!(error.code(), Some("cli.usage"));
        assert_eq!(error.exit_code(), exit::USAGE);
        assert_eq!(error.usage(), Some("usage: mirvm pack <target>\n"));
        assert_eq!(
            diag::render_for_test(&error),
            "mirvm[pack]: error: unknown argument `--x`\n"
        );
    }

    #[test]
    fn an_unscoped_failure_claims_no_component() {
        let error = Error::usage_unscoped("MIRVM_THREADS is invalid");
        assert_eq!(error.component(), None);
        assert_eq!(
            diag::render_for_test(&error),
            "mirvm: error: MIRVM_THREADS is invalid\n"
        );
    }

    #[test]
    fn a_module_error_keeps_its_own_identity_through_the_root() {
        let error = Error::from(crate::sysroot::Error::StampUnavailable);
        assert_eq!(error.component(), Some(Component::Sysroot));
        assert_eq!(error.code(), Some("sysroot.stamp_unavailable"));
        assert_eq!(error.exit_code(), exit::FAILURE);

        let usage = Error::from(crate::options::Error::DepsTrack {
            env: crate::options::env_var_name("deps"),
            value: "invalid".into(),
        });
        assert_eq!(usage.component(), Some(Component::Options));
        assert_eq!(usage.code(), Some("options.deps_track"));
        assert_eq!(usage.exit_code(), exit::USAGE);
        assert_eq!(
            diag::render_for_test(&usage),
            "mirvm[options]: error: MIRVM_DEPS only accepts `cargo` or `self` (got `invalid`)\n"
        );
    }
}
