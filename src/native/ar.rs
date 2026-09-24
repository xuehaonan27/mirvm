//! The Unix `ar` archive container.
//!
//! A static library is a flat member list behind a fixed 60-byte header record, with the symbol
//! table and the long-name table carried as members of their own. Walking it is format knowledge,
//! so the walk lives here and the callers ask for member payloads; what a payload means — an ELF
//! object, a text listing, nothing at all — is theirs to decide.

use std::fmt;

/// The eight bytes an `ar` archive begins with.
pub const SIGNATURE: &[u8; 8] = b"!<arch>\n";

/// The fixed member header record: `name[16] date[12] uid[6] gid[6] mode[8] size[10] "`\n"`.
pub const HEADER_SIZE: usize = 60;

/// The two bytes a member header record ends with.
const HEADER_MAGIC: &[u8; 2] = b"`\n";

/// Why an `ar` archive could not be walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Malformed {
    /// The file signature is missing.
    NotAnArchive,
    /// A member header's trailing magic is not where the header chain says the header is.
    HeaderMagic { at: usize },
    /// A member header's size field is not ASCII.
    SizeNotAscii,
    /// A member header's size field is not a number.
    SizeUnparsable { text: String },
    /// A member's declared size runs past the end of the file.
    BodyOutOfBounds,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::NotAnArchive => f.write_str("not a Unix ar archive"),
            Malformed::HeaderMagic { at } => write!(f, "ar member header magic misplaced @{at:#x}"),
            Malformed::SizeNotAscii => f.write_str("ar member size is not ASCII"),
            Malformed::SizeUnparsable { text } => {
                write!(f, "ar member size is unparsable `{text}`")
            }
            Malformed::BodyOutOfBounds => f.write_str("ar member body out of bounds"),
        }
    }
}

impl std::error::Error for Malformed {}

/// The payload of every real member, in file order, with the metadata members left out and the
/// embedded long name already stripped.
///
/// A trailing run shorter than a header record is ignored, which is what makes a truncated
/// archive's last partial member invisible rather than fatal; any *declared* body that runs past
/// the end is an error.
pub fn members(bytes: &[u8]) -> Result<Vec<&[u8]>, Malformed> {
    if !bytes.starts_with(SIGNATURE) {
        return Err(Malformed::NotAnArchive);
    }
    let mut out = Vec::new();
    let mut position = SIGNATURE.len();
    while position + HEADER_SIZE <= bytes.len() {
        let header = &bytes[position..position + HEADER_SIZE];
        if &header[58..60] != HEADER_MAGIC {
            return Err(Malformed::HeaderMagic { at: position });
        }
        let size_text =
            std::str::from_utf8(&header[48..58]).map_err(|_| Malformed::SizeNotAscii)?;
        let size: usize = size_text
            .trim()
            .parse()
            .map_err(|_| Malformed::SizeUnparsable {
                text: size_text.to_string(),
            })?;
        let body_end = position + HEADER_SIZE + size;
        if body_end > bytes.len() {
            return Err(Malformed::BodyOutOfBounds);
        }
        let raw_name = String::from_utf8_lossy(&header[0..16]).trim().to_string();
        let mut body = &bytes[position + HEADER_SIZE..body_end];
        // The `#1/<len>` form: the name is embedded at the start of the body, so its length comes
        // off before the content, and the name it hides is the one that says whether this is a
        // member at all. GNU writes `/#1/<len>` and the other `ar` writes `#1/<len>`, so the
        // leading slash is optional rather than part of the form.
        let embedded = raw_name
            .strip_prefix('/')
            .unwrap_or(&raw_name)
            .strip_prefix("#1/")
            .and_then(|rest| rest.trim().parse::<usize>().ok());
        let name = match embedded {
            Some(length) => {
                let (name, content) = body.split_at(length.min(body.len()));
                body = content;
                String::from_utf8_lossy(name)
                    .trim_matches(|character| character == '\0' || character == ' ')
                    .to_string()
            }
            None => raw_name,
        };
        if !is_metadata(&name) {
            out.push(body);
        }
        // Member bodies are aligned to 2, so an odd size is followed by one pad byte.
        position = body_end + (size & 1);
    }
    Ok(out)
}

/// The members that carry metadata rather than payload: the symbol table (`/`, `__.SYMDEF`,
/// `/SYM64/`) and the string table (`//`).
///
/// This has to be exact. GNU `ar` references a name longer than 15 characters through the string
/// table as `/N` (e.g. `/0`), so a leading `/` does **not** by itself mean metadata.
fn is_metadata(name: &str) -> bool {
    matches!(
        name,
        "/" | "//" | "__.SYMDEF" | "__.SYMDEF SORTED" | "/SYM64/"
    )
}
