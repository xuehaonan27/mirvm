//! Machine rendering: one JSON object per diagnostic line.
//!
//! The envelope is fixed and versioned. `details` is a pre-rendered object produced by the type that
//! owns the fields, so this module never needs to know any error's shape — it splices the fragment
//! verbatim. That keeps the diagnostic vocabulary free of serialization dependencies, which the TSan
//! harness relies on.

use std::fmt::Write as _;

use super::Diagnostic;

/// The envelope version. A consumer may reject a version it does not know before reading further.
const VERSION: u8 = 1;

/// Whether `MIRVM_OUTPUT` selects machine output.
///
/// Read live rather than snapshotted: the command line writes its decision into the same variable
/// (the `--stack-size`/`--jit` discipline), and a diagnostic can be emitted before the parser has
/// seen the flag.
pub(super) fn enabled() -> bool {
    std::env::var_os(crate::options::env_var_name("output_format"))
        .is_some_and(|value| value == "json")
}

/// One JSON line for a diagnostic, newline included.
pub(super) fn line(diagnostic: &dyn Diagnostic) -> String {
    let mut out = String::with_capacity(192);
    let _ = write!(
        out,
        "{{\"v\":{VERSION},\"severity\":\"{}\",\"component\":",
        diagnostic.severity().name()
    );
    match diagnostic.component() {
        Some(component) => string(component.name(), &mut out),
        None => out.push_str("null"),
    }
    out.push_str(",\"code\":");
    match diagnostic.code() {
        Some(code) => string(code, &mut out),
        None => out.push_str("null"),
    }
    out.push_str(",\"message\":");
    string(&diagnostic.to_string(), &mut out);
    out.push_str(",\"details\":");
    match diagnostic.details() {
        Some(details) => out.push_str(&details),
        None => out.push_str("null"),
    }
    out.push_str(",\"causes\":[");
    for (index, cause) in diagnostic.causes().iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        string(cause, &mut out);
    }
    out.push(']');
    // Only a failure has an exit code; an event's `Kind` is meaningless and must not be reported as
    // if the process were about to exit with it.
    if diagnostic.severity() == super::Severity::Error {
        let _ = write!(out, ",\"exit_code\":{}", diagnostic.kind().code());
    }
    out.push_str("}\n");
    out
}

/// Append one JSON string literal.
fn string(value: &str, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
