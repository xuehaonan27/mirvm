//! The entry mechanism the generational cache layers share: how an entry key is built, when a
//! freshly lowered snapshot may be published, and what a loaded one must satisfy before it is used.
//!
//! Each layer keeps its own key material and its own header payload — a base image records the
//! sysroot stamp, a dependency image the `--extern` stamps, the L2 entry the rustc arguments — so
//! what lives here is only what is identical in all three: the separator convention, the
//! generation check, and the guards that decide whether a snapshot's embedded absolute addresses
//! are valid in another process.

use std::path::Path;

use crate::vm::instance::Instance;
use crate::vm::{ir, verify};

/// Separator between key parts (ASCII unit separator). `cargoless` builds its own composite keys
/// with the same byte; the two grammars never mix, but neither may change without invalidating
/// every warm entry.
const FIELD_SEP: char = '\u{1f}';

/// The key of one entry: the build id followed by the parts that identify the product.
///
/// The separator is what keeps `["ab"]` and `["a", "b"]` apart. A part that itself contains the
/// separator is not escaped, so a part list is only as unambiguous as its material: paths and rustc
/// arguments never contain it.
pub(crate) struct Key(String);

impl Key {
    /// A key always starts with the build id: an entry is never reused across mirvm builds.
    pub(crate) fn new() -> Self {
        Key(crate::options::build::BUILD_ID.to_string())
    }

    /// Append one part.
    pub(crate) fn part(&mut self, part: &str) -> &mut Self {
        self.0.push(FIELD_SEP);
        self.0.push_str(part);
        self
    }

    /// The key text, as a layer records it in its key chain.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The key as a file name stem.
    pub(crate) fn digest(&self) -> String {
        format!("{:016x}", crate::utils::content::fnv1a(self.0.as_bytes()))
    }
}

/// Whether a generation recorded in an entry belongs to this build.
pub(crate) fn is_current_generation(generation: &str) -> bool {
    generation == crate::options::build::BUILD_ID
}

/// Whether a freshly lowered snapshot may be written into the store.
///
/// The snapshot embeds absolute addresses, so it is valid in another process only when the frozen
/// area sits at a fixed base: for a delta at *any* fixed base (the file records its own domain), and
/// for a base or dependency image at the fixed domain the layer below expects it in. An fn-ptr value
/// is a stub code address, so a module that has entry stubs needs that area pinned too.
pub(crate) fn snapshot_is_publishable(
    module: &ir::Module,
    instance: &Instance,
    domain: Option<usize>,
) -> bool {
    frozen_at(module, domain) && entry_stubs_pinned(module, instance)
}

/// The other half of the same rule, for a caller that wants to name which half failed: an fn-ptr
/// value is a stub code address, so the stub area must be pinned when the module has one.
pub(crate) fn entry_stubs_pinned(module: &ir::Module, instance: &Instance) -> bool {
    module.entry_stub_sites.is_empty() || instance.entry_stubs.at_fixed_base()
}

/// The load side of the same rule: whether a loaded snapshot really landed in `domain` (`None`: any
/// fixed base). A taken domain and a swapped file are rejected alike. The artifact carries the bytes
/// only when they were linked against a fixed base, so the recorded home is the whole answer.
pub(crate) fn frozen_at(module: &ir::Module, domain: Option<usize>) -> bool {
    module
        .frozen
        .as_ref()
        .is_some_and(|frozen| domain.is_none_or(|home| frozen.home() == home))
}

/// Whether the native modules a snapshot needs on disk are still there. A missing one is a miss,
/// never a runtime error: the cold path rebuilds and self-heals.
pub(crate) fn native_libs_present(module: &ir::Module) -> bool {
    module
        .required_native_libs
        .iter()
        .all(|path| Path::new(&**path).is_file())
}

/// Bring a loaded snapshot back to life: map the frozen bytes and derive the instance tables the
/// file does not carry, then verify the module against the stack it is about to join (or against
/// nothing, for the layer that starts the stack). `None` is a miss — an unverified module is never
/// used.
pub(crate) fn revive(module: &mut ir::Module, prefix: verify::Prefix) -> Option<Instance> {
    let instance = Instance::materialize(module).ok()?;
    verify::module_with_prefix(module, &instance, prefix)
        .ok()
        .map(|()| instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_starts_with_the_build_id_and_separates_parts() {
        // A part boundary cannot be forged by concatenation: this is what the separator buys.
        let mut joined = Key::new();
        joined.part("ab");
        let mut split = Key::new();
        split.part("a").part("b");
        assert_ne!(joined.as_str(), split.as_str());

        assert!(split.as_str().starts_with(crate::options::build::BUILD_ID));
        assert_eq!(split.digest().len(), 16);
        assert!(split.digest().chars().all(|c| c.is_ascii_hexdigit()));
        // Two different part lists never share a file name in practice.
        assert_ne!(joined.digest(), split.digest());
    }

    #[test]
    fn key_text_is_the_build_id_then_the_separator_then_the_parts() {
        // The file name of an entry is a digest of this text, so the grammar itself is a contract:
        // changing it silently invalidates every warm entry.
        let mut key = Key::new();
        key.part("x").part("y");
        assert_eq!(
            key.as_str(),
            format!("{}\u{1f}x\u{1f}y", crate::options::build::BUILD_ID)
        );
    }

    #[test]
    fn only_this_build_is_current() {
        assert!(is_current_generation(crate::options::build::BUILD_ID));
        assert!(!is_current_generation("0000000000000000"));
        assert!(!is_current_generation(""));
    }
}
