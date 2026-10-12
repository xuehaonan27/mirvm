//! Shared-pointer validation with a bounded nesting depth for recursive IR operands.

use std::alloc::{Layout, LayoutError};
use std::any::TypeId;

use rkyv::validation::validators::{ArchiveValidator, DefaultValidatorError, SharedValidator};
use rkyv::validation::{ArchiveContext, SharedContext};

pub(super) struct Validator<'a> {
    archive: ArchiveValidator<'a>,
    shared: SharedValidator,
}

impl<'a> Validator<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self {
            archive: ArchiveValidator::with_max_depth(bytes, 128),
            shared: SharedValidator::new(),
        }
    }
}

impl rkyv::Fallible for Validator<'_> {
    type Error = DefaultValidatorError;
}

macro_rules! checked {
    (unsafe fn $name:ident($($arg:ident: $ty:ty),*) -> $result:ty) => {
        unsafe fn $name(&mut self, $($arg: $ty),*) -> Result<$result, Self::Error> {
            // SAFETY: the caller's ArchiveContext preconditions pass unchanged to the validator.
            unsafe { self.archive.$name($($arg),*) }.map_err(DefaultValidatorError::ArchiveError)
        }
    };
    (fn $name:ident($($arg:ident: $ty:ty),*) -> $result:ty) => {
        fn $name(&mut self, $($arg: $ty),*) -> Result<$result, Self::Error> {
            self.archive.$name($($arg),*).map_err(DefaultValidatorError::ArchiveError)
        }
    };
}

impl<'a> ArchiveContext for Validator<'a> {
    type PrefixRange = <ArchiveValidator<'a> as ArchiveContext>::PrefixRange;
    type SuffixRange = <ArchiveValidator<'a> as ArchiveContext>::SuffixRange;

    checked!(unsafe fn bounds_check_ptr(base: *const u8, offset: isize) -> *const u8);
    checked!(unsafe fn bounds_check_layout(data_address: *const u8, layout: &Layout) -> ());
    checked!(unsafe fn bounds_check_subtree_ptr_layout(data_address: *const u8, layout: &Layout) -> ());
    checked!(unsafe fn push_prefix_subtree_range(root: *const u8, end: *const u8) -> Self::PrefixRange);
    checked!(fn pop_prefix_range(range: Self::PrefixRange) -> ());
    checked!(unsafe fn push_suffix_subtree_range(start: *const u8, root: *const u8) -> Self::SuffixRange);
    checked!(fn pop_suffix_range(range: Self::SuffixRange) -> ());
    checked!(fn finish() -> ());

    fn wrap_layout_error(error: LayoutError) -> Self::Error {
        DefaultValidatorError::ArchiveError(ArchiveValidator::wrap_layout_error(error))
    }
}

impl SharedContext for Validator<'_> {
    fn register_shared_ptr(
        &mut self,
        ptr: *const u8,
        type_id: TypeId,
    ) -> Result<bool, Self::Error> {
        self.shared
            .register_shared_ptr(ptr, type_id)
            .map_err(DefaultValidatorError::SharedError)
    }
}
