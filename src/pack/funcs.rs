//! The FUNCS section is the same checked, immutable function archive used by execution images.

use super::Error;
use crate::vm::ir::FuncTable;

pub(super) fn build_function_section(funcs: &FuncTable) -> Result<Vec<u8>, Error> {
    crate::vm::archive::functions::encode(funcs)
        .map(|bytes| bytes.to_vec())
        .map_err(|error| Error::build(error.to_string()))
}
