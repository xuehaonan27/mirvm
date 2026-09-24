//! The assembler source vocabulary an object format reads.
//!
//! The materializers in `src/lower/` and the rlib rescue in [`super::archive`] emit assembler source
//! and hand it to the platform's toolchain. Most of that source is either an instruction, which is
//! the CPU's and comes from [`crate::arch`], or a sequence of statements, which the materializer
//! owns; what is neither is how one statement is *spelled*. A directive that declares a symbol,
//! places it in a region, or keeps it out of the other images' namespace belongs to the object format
//! alone: ELF says `.hidden`/`.type`/`.size` and Mach-O has no counterpart for any of the three, so
//! it says `.private_extern` instead; Mach-O additionally gives every symbol the leading underscore
//! that its symbol table carries, in source and in the table alike.
//!
//! None of that varies with the kernel or the CPU, which is why it is here beside the two byte
//! layouts rather than on an axis. The format arrives as a value from `crate::os::dll`, the same way
//! [`super::symtab`] takes it.
//!
//! Two spellings deliberately stay with the call site: `.balign` and `.quad` mean the same thing to
//! both assemblers, and the order the statements come in is what the materializer is for.

use std::fmt::Write as _;

use crate::os::dll::ObjectFormat;

/// Whether a symbol an image defines is reachable from the other images the process loads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// Named in the image's own dynamic symbol table, so another image can bind to it.
    Exported,
    /// Defined in this image and reachable only from this image.
    Private,
}

/// A region of the image that emitted statements are placed in, and that they return out of.
pub(crate) enum Region<'a> {
    /// Executable code. `name` is the symbol the region holds, which a format that gives every
    /// function a section of its own reads as the section's name and a format that does not
    /// ignores.
    Text { name: &'a str },
    /// The private writable region `name` of 8-byte slots, which the owning Engine patches after
    /// the image is loaded and no other image can see.
    Slots { name: &'a str },
}

/// One object format's spelling of the statements the materializers emit.
pub(crate) struct Vocabulary {
    format: ObjectFormat,
}

impl Vocabulary {
    pub(crate) fn of(format: ObjectFormat) -> Self {
        Vocabulary { format }
    }

    /// `name` as this format's assembler source spells it.
    ///
    /// The spelling is not cosmetic: a `sym` operand and a symbol this image defines are matched
    /// against the names in rustc's own objects, so a reference spelled the format's way and a
    /// definition spelled Rust's way do not meet.
    pub(crate) fn symbol(&self, name: &str) -> String {
        match self.format {
            ObjectFormat::Elf => name.to_string(),
            ObjectFormat::MachO => format!("_{name}"),
        }
    }

    /// Begin `region`, remembering the region it interrupts.
    pub(crate) fn open(&self, out: &mut String, region: Region<'_>) {
        match (self.format, region) {
            (ObjectFormat::Elf, Region::Text { name }) => {
                let _ = writeln!(out, ".pushsection .text.{name},\"ax\", @progbits");
            }
            (ObjectFormat::Elf, Region::Slots { name }) => {
                let _ = writeln!(out, ".pushsection .data.{name},\"aw\",@progbits");
            }
            (ObjectFormat::MachO, Region::Text { .. }) => {
                let _ = writeln!(out, ".pushsection __TEXT,__text,regular,pure_instructions");
            }
            (ObjectFormat::MachO, Region::Slots { name }) => {
                let _ = writeln!(out, ".pushsection __DATA,{name}");
            }
        }
    }

    /// Return to the region [`Self::open`] interrupted.
    pub(crate) fn close(&self, out: &mut String) {
        out.push_str(".popsection\n");
    }

    /// Declare `name` a function this image defines, and begin its body.
    pub(crate) fn define_fn(&self, out: &mut String, name: &str, visibility: Visibility) {
        let name = self.symbol(name);
        let _ = writeln!(out, ".globl {name}");
        match (self.format, visibility) {
            (ObjectFormat::Elf, Visibility::Private) => {
                let _ = writeln!(out, ".hidden {name}");
            }
            (ObjectFormat::MachO, Visibility::Private) => {
                let _ = writeln!(out, ".private_extern {name}");
            }
            (_, Visibility::Exported) => {}
        }
        if self.format == ObjectFormat::Elf {
            let _ = writeln!(out, ".type {name},@function");
        }
        let _ = writeln!(out, "{name}:");
    }

    /// End the function `name`, which [`Self::define_fn`] began.
    pub(crate) fn end_fn(&self, out: &mut String, name: &str) {
        if self.format == ObjectFormat::Elf {
            let _ = writeln!(out, ".size {name}, . - {name}");
        }
    }

    /// Declare `name` an 8-byte object of this image, zero, and emit its label.
    pub(crate) fn define_slot(&self, out: &mut String, name: &str, visibility: Visibility) {
        let name = self.symbol(name);
        let _ = writeln!(out, ".globl {name}");
        match (self.format, visibility) {
            (ObjectFormat::Elf, Visibility::Private) => {
                let _ = writeln!(out, ".hidden {name}");
            }
            (ObjectFormat::MachO, Visibility::Private) => {
                let _ = writeln!(out, ".private_extern {name}");
            }
            (_, Visibility::Exported) => {}
        }
        if self.format == ObjectFormat::Elf {
            let _ = writeln!(out, ".type {name},@object");
            let _ = writeln!(out, ".size {name},8");
        }
        let _ = writeln!(out, "{name}:");
        let _ = writeln!(out, "    .quad 0");
    }
}
