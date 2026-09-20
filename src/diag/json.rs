//! Machine rendering: one JSON object per diagnostic line, and the report documents.
//!
//! Every mirvm-authored JSON document is assembled by [`Writer`], so the escaping, the separator and
//! the version field exist in one place. A report that builds its own braces and commas is how two
//! documents end up with different ideas of what a string may contain.
//!
//! This module is compiled source-for-source into the TSan harness, so it must stay pure `std`.

use std::fmt::Write as _;

use super::Diagnostic;

/// The document version. A consumer may reject a version it does not know before reading further.
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

/// Assemble one JSON object.
pub(crate) struct Writer {
    out: String,
}

impl Writer {
    /// A nested object: no version field, because the document that contains it carries one.
    pub(crate) fn new() -> Self {
        Self {
            out: String::from("{"),
        }
    }

    /// A top-level document: the version comes first, so a consumer can reject what it does not know.
    pub(crate) fn document() -> Self {
        Self {
            out: format!("{{\"v\":{VERSION}"),
        }
    }

    pub(crate) fn string(&mut self, name: &str, value: &str) -> &mut Self {
        self.key(name);
        escape(value, &mut self.out);
        self
    }

    pub(crate) fn number(&mut self, name: &str, value: u64) -> &mut Self {
        self.key(name);
        let _ = write!(self.out, "{value}");
        self
    }

    pub(crate) fn boolean(&mut self, name: &str, value: bool) -> &mut Self {
        self.key(name);
        self.out.push_str(if value { "true" } else { "false" });
        self
    }

    pub(crate) fn null(&mut self, name: &str) -> &mut Self {
        self.key(name);
        self.out.push_str("null");
        self
    }

    /// A value the caller already rendered: a nested object, or an array built by [`array`].
    pub(crate) fn raw(&mut self, name: &str, value: &str) -> &mut Self {
        self.key(name);
        self.out.push_str(value);
        self
    }

    pub(crate) fn finish(mut self) -> String {
        self.out.push_str("}\n");
        self.out
    }

    fn key(&mut self, name: &str) {
        if self.out.len() > 1 {
            self.out.push(',');
        }
        self.out.push('"');
        self.out.push_str(name);
        self.out.push_str("\":");
    }
}

/// A JSON array of already-rendered values.
pub(crate) fn array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(item);
    }
    out.push(']');
    out
}

/// One JSON string literal, for a value spliced into an array or a nested object.
pub(crate) fn literal(value: &str) -> String {
    let mut out = String::new();
    escape(value, &mut out);
    out
}

/// One JSON line for a diagnostic, newline included.
pub(super) fn line(diagnostic: &dyn Diagnostic) -> String {
    let mut out = Writer::document();
    out.string("severity", diagnostic.severity().name());
    match diagnostic.component().or_else(super::current) {
        Some(component) => out.string("component", component.name()),
        None => out.null("component"),
    };
    match diagnostic.code() {
        Some(code) => out.string("code", code),
        None => out.null("code"),
    };
    out.string("message", &diagnostic.to_string());
    match diagnostic.details() {
        Some(details) => out.raw("details", &details),
        None => out.null("details"),
    };
    let causes: Vec<String> = diagnostic.causes().iter().map(|c| literal(c)).collect();
    out.raw("causes", &array(&causes));
    match diagnostic.usage() {
        Some(usage) => out.string("usage", usage),
        None => out.null("usage"),
    };
    // Only a failure has an exit code; an event's `Kind` is meaningless and must not be reported as
    // if the process were about to exit with it.
    if diagnostic.severity() == super::Severity::Error {
        out.number("exit_code", diagnostic.kind().code().into());
    }
    out.finish()
}

/// Append one JSON string literal.
fn escape(value: &str, out: &mut String) {
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
