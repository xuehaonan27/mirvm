/// Why lowering could not produce the bytecode it was asked for.
///
/// The layer's own boundary is the important split: `Unsupported` means the program uses something
/// this lowerer does not implement (the `M4.x`, `C1` and `F-06` boundaries named in the detail), so
/// the answer is at the product boundary; `Internal` means an assumption this layer makes about the
/// MIR or the ABI shape did not hold, so the answer is in this tree. `Assemble` is the asm-recipe
/// path (`cc`, `dlopen`, `dlsym`) and `Io` is a filesystem step, with the `io::Error` as its source.
///
/// A `Trap(diagnostic)` is *not* one of these: an unexecuted path may stay a trap, so an
/// unrecognized construct only becomes an error when something asks for it.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("{detail}")]
    Unsupported { detail: String },

    #[error("{detail}")]
    Internal { detail: String },

    #[error("{detail}")]
    Assemble { detail: String },

    /// A `sym` in a dependency's asm points at that dependency's own guest function, whose
    /// cross-crate entry budget belongs to the bin link context and is not implemented: the
    /// dependency's manifest is skipped rather than failing its build. This is a separate variant
    /// because the caller acts on it, which used to be a string prefix test.
    #[error("{detail}")]
    DepGuestSym { detail: String },

    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Lower => {
        Unsupported => "lower.unsupported",
        Internal => "lower.internal",
        Assemble => "lower.assemble",
        DepGuestSym => "lower.dep_guest_sym",
        Io => "lower.io",
    }
}

impl Error {
    pub(crate) fn unsupported(detail: impl Into<String>) -> Self {
        Error::Unsupported {
            detail: detail.into(),
        }
    }

    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Error::Internal {
            detail: detail.into(),
        }
    }

    pub(crate) fn assemble(detail: impl Into<String>) -> Self {
        Error::Assemble {
            detail: detail.into(),
        }
    }

    pub(crate) fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}
