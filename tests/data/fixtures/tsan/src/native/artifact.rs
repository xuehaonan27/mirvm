//! The one artifact this harness can carry.
//!
//! The real module also holds the archive converter, whose rustc session is what keeps it out of
//! this build, and the bridge artifact, which needs a `cc` and a loaded image.

#[path = "../../../../../../src/native/artifact/asmtext.rs"]
pub(crate) mod asmtext;
