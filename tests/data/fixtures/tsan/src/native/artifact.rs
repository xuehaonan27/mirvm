//! Compiler-independent assembly and bridge materialization, source-shared with the runtime.

#[path = "../../../../../../src/native/artifact/asmtext.rs"]
pub(crate) mod asmtext;

#[path = "../../../../../../src/native/artifact/assembly.rs"]
pub(crate) mod assembly;
#[path = "../../../../../../src/native/artifact/bridge.rs"]
pub(crate) mod bridge;
