//! Native images mapped through the system loader, and the process-lifetime ownership of the
//! code they contain.
//!
//! Four concerns, one home each: this file is the dlopen'd image [`NativeImage`] and the
//! executable ranges that stay attributable after the Engine that produced them is gone;
//! [`identity`] gives each Engine its own private file identity; [`open`] maps and relocates an
//! object with its constructors suppressed; and [`wire`] fills the bridge slots the image calls
//! through, commits it for the process, and runs its constructors and destructors.
//!
//! The self-mapping path -- a package's own `global_asm`/`dep_asm` objects, loaded without
//! dlopen -- is [`super::mcload`].

mod identity;
mod open;
mod wire;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use super::native_lifecycle::NativeLifecycle;

pub(crate) use identity::isolate_required_libraries;
pub(crate) use open::{open_for_lower, prepare_required_libraries};
pub(crate) use wire::{
    commit_images, patch_entry_slots, patch_pthread_slots, run_finalizers, run_initializers,
};

/// A mapped self-produced shared object. The system loader performs dependency
/// resolution and ELF relocation, but mirvm controls init/fini timing so P1
/// callback slots are valid before any constructor can call guest code.
#[derive(Debug)]
#[doc(hidden)]
pub struct NativeImage {
    handle: usize,
    bias: u64,
    hidden_symbols: HashMap<Box<str>, u64>,
    executable_ranges: Box<[(usize, usize)]>,
    lifecycle: NativeLifecycle,
    committed: AtomicBool,
}

impl NativeImage {
    pub(crate) fn handle(&self) -> usize {
        self.handle
    }

    pub(crate) fn bias(&self) -> u64 {
        self.bias
    }

    pub(crate) fn hidden_symbol_values(&self) -> &HashMap<Box<str>, u64> {
        &self.hidden_symbols
    }

    fn executable_ranges(&self) -> &[(usize, usize)] {
        &self.executable_ranges
    }
}

struct OwnedNativeCode {
    start: usize,
    end: usize,
    control: Arc<super::ctx::EngineControl>,
}

/// Process-lifetime ownership for executable code emitted from guest inputs.
/// Committed images are intentionally never unmapped, so an address returned
/// through `oldact` remains attributable even after its Engine has closed.
static OWNED_NATIVE_CODE: LazyLock<RwLock<Vec<OwnedNativeCode>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub(crate) fn owner_of_executable_address(
    address: usize,
) -> Option<Arc<super::ctx::EngineControl>> {
    OWNED_NATIVE_CODE
        .read()
        .unwrap()
        .iter()
        .rev()
        .find(|range| address >= range.start && address < range.end)
        .map(|range| Arc::clone(&range.control))
}

// Native function addresses and dlopen handles are immutable after publication.
unsafe impl Send for NativeImage {}
unsafe impl Sync for NativeImage {}

impl Drop for NativeImage {
    fn drop(&mut self) {
        if !self.committed.load(Ordering::Acquire) {
            unsafe { crate::os::dll::close(self.handle) };
        }
    }
}
