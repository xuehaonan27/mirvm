//! The image's `.eh_frame`: the unwind records the runtime's unwinder needs to step through
//! guest code.
//!
//! Records are parsed while the mapping is still writable, but the FDE addresses are published to
//! the unwinder only after every fallible segment-protection step has succeeded, so a load that
//! fails leaves no record registered for a mapping about to be dropped.

use super::bad;
use super::map::Mapping;
use super::parse::Tables;

/// The address of every FDE, which is what registers a function's unwind information. A CIE --
/// a record whose `cie_pointer` field is zero -- is not a registration point; the unwinder reaches
/// it through the FDEs that reference it.
pub(super) fn frames(mapping: &Mapping, tables: &Tables) -> Result<Box<[usize]>, String> {
    let Some(eh_frame) = tables.eh_frame else {
        return Ok(Vec::new().into_boxed_slice());
    };
    if !mapping.contains(eh_frame.addr, eh_frame.size) {
        return Err("MC .eh_frame lies outside a loadable segment".into());
    }
    let start = mapping.address(eh_frame.addr, ".eh_frame")?;
    let end = start
        .checked_add(usize::try_from(eh_frame.size).map_err(|_| bad())?)
        .ok_or_else(bad)?;
    let mut frames = Vec::new();
    let mut cursor = start;
    while cursor + 8 <= end {
        let length =
            u32::from_le_bytes(unsafe { std::ptr::read(cursor as *const [u8; 4]) }) as usize;
        if length == 0 {
            break;
        }
        let record_end = cursor
            .checked_add(length + 4)
            .ok_or_else(|| "MC .eh_frame record overflow".to_string())?;
        if record_end > end {
            return Err("MC .eh_frame record exceeds section".into());
        }
        let cie_pointer =
            u32::from_le_bytes(unsafe { std::ptr::read((cursor + 4) as *const [u8; 4]) });
        if cie_pointer != 0 {
            frames.push(cursor);
        }
        cursor = record_end;
    }
    Ok(frames.into_boxed_slice())
}

/// Publish the parsed records to the running unwinder.
pub(super) fn register(frames: &[usize]) {
    for &frame in frames {
        crate::os::unwind::register_frame(frame as *const u8);
    }
}

/// Take them back, newest first, which is the reverse of `register`.
pub(super) fn deregister(frames: &[usize]) {
    for &frame in frames.iter().rev() {
        crate::os::unwind::deregister_frame(frame as *const u8);
    }
}
