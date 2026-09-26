//! The names in a Mach-O image's symbol table.
//!
//! Every entry is an `nlist_64` against a string table, and the format's leading underscore on a C
//! name is dropped here so that a caller compares the same string it would use anywhere else. What
//! an entry's bits mean is the layout's (`crate::native::object::macho`); this module is the one
//! place that turns them into "the loader resolves this", "the image hides this" and "the image
//! offers this".

use crate::native::object::macho::{
    HEADER_SIZE, LC_SYMTAB, N_EXT, N_PEXT, N_TYPE, N_UNDF, N_WEAK_DEF, NLIST_SIZE,
};
use crate::native::object::macho::{header, load_command};
use crate::utils::bytes::{read_u32, read_u64};

/// One symbol of an image's table.
pub(crate) struct ImageSymbol {
    /// The name as the rest of mirvm spells it: this format's leading underscore is dropped, so a
    /// caller compares the same string it would use anywhere else.
    pub(crate) name: Box<str>,
    /// The address the symbol is defined at in the image, or zero for one the loader resolves.
    pub(crate) value: u64,
    /// Whether the loader has to resolve it rather than the image defining it.
    pub(crate) undefined: bool,
    /// Whether the image marks it private, which is how this format says the loader cannot reach
    /// it by name even though the image defines it.
    pub(crate) private_extern: bool,
    /// Whether the image defines it and offers it to other images.
    pub(crate) exported: bool,
    /// Whether the image marks it a weak definition, which is how this format says two images may
    /// define one name and the linker picks either.
    pub(crate) weak: bool,
}

/// The symbols of a Mach-O image, in the order its table lists them.
pub(crate) fn symbols(bytes: &[u8]) -> Result<Vec<ImageSymbol>, String> {
    let (commands, _) = header(bytes)?;
    let mut cursor = HEADER_SIZE;
    for _ in 0..commands {
        let (command, size) = load_command(bytes, cursor)?;
        if command == LC_SYMTAB {
            return symbol_table(bytes, symtab_at(bytes, cursor)?);
        }
        cursor = cursor
            .checked_add(size as usize)
            .ok_or_else(|| "the load commands overrun the image".to_string())?;
    }
    Err("the image carries no symbol table".to_string())
}

/// The names of the symbols the loader has to resolve, which is what an image's own undefined
/// range is: the external symbols its table defines nowhere.
pub(crate) fn undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    Ok(symbols(bytes)?
        .into_iter()
        .filter(|symbol| symbol.undefined)
        .map(|symbol| symbol.name)
        .collect())
}

/// The symbols the image defines but does not export, by name and address.
///
/// An image exports every external symbol it does not mark private, so this is the set a caller
/// cannot reach by name through the loader however the image is loaded.
pub(crate) fn hidden_symbols(bytes: &[u8]) -> Result<Vec<(Box<str>, u64)>, String> {
    Ok(symbols(bytes)?
        .into_iter()
        .filter(|symbol| !symbol.undefined && symbol.private_extern)
        .map(|symbol| (symbol.name, symbol.value))
        .collect())
}

/// The `LC_SYMTAB` fields at `cursor`: `symoff`, `nsyms`, `stroff` and `strsize`.
pub(crate) fn symtab_at(bytes: &[u8], cursor: usize) -> Result<[usize; 4], String> {
    let short = || "the image ends inside a load command".to_string();
    let mut fields = [0_usize; 4];
    for (index, field) in fields.iter_mut().enumerate() {
        let at = cursor + 8 + index * 4;
        *field = read_u32(bytes, at).ok_or_else(short)? as usize;
    }
    Ok(fields)
}

/// Walks `[symoff, nsyms, stroff, strsize]` into the symbols they name.
fn symbol_table(bytes: &[u8], fields: [usize; 4]) -> Result<Vec<ImageSymbol>, String> {
    let [symoff, nsyms, stroff, strsize] = fields;
    let strings = bytes
        .get(stroff..stroff + strsize)
        .ok_or_else(|| "the string table lies outside the image".to_string())?;
    let mut out = Vec::with_capacity(nsyms);
    for index in 0..nsyms {
        let at = symoff + index * NLIST_SIZE;
        let entry = bytes
            .get(at..at + NLIST_SIZE)
            .ok_or_else(|| "the symbol table lies outside the image".to_string())?;
        // `nlist_64` is `n_strx`, `n_type`, `n_sect`, `n_desc`, `n_value`.
        let name = name_of(
            strings,
            read_u32(bytes, at)
                .ok_or_else(|| "the symbol table lies outside the image".to_string())?
                as usize,
        )?;
        out.push(ImageSymbol {
            name,
            value: read_u64(entry, 8).ok_or_else(|| "a symbol entry is truncated".to_string())?,
            undefined: entry[4] & N_TYPE == N_UNDF && entry[4] & N_EXT != 0,
            private_extern: entry[4] & N_PEXT != 0,
            exported: entry[4] & N_TYPE != N_UNDF
                && entry[4] & N_EXT != 0
                && entry[4] & N_PEXT == 0,
            weak: u16::from_le_bytes([entry[6], entry[7]]) & N_WEAK_DEF != 0,
        });
    }
    Ok(out)
}

/// The symbol name at `offset` in the string table, without this format's leading underscore.
fn name_of(strings: &[u8], offset: usize) -> Result<Box<str>, String> {
    let tail = strings
        .get(offset..)
        .ok_or_else(|| "a symbol name lies outside the string table".to_string())?;
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| "a symbol name is not terminated".to_string())?;
    let name =
        std::str::from_utf8(&tail[..end]).map_err(|_| "a symbol name is not UTF-8".to_string())?;
    Ok(Box::from(name.strip_prefix('_').unwrap_or(name)))
}
