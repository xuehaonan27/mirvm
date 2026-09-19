//! A crate used to pin Cargo and rustdoc doctest behavior.
//!
//! ```
//! assert_eq!(cless_doctest_contract::library_value(), "from-build-rs");
//! assert!(cless_doctest_contract::build_cfg_present());
//! assert_eq!(doctest_helper::value(), "dev-helper");
//! assert!(cfg!(debug_assertions));
//! assert_ne!(std::env::var("CLESS_DOCTEST_FORCE_FAIL").as_deref(), Ok("1"));
//! ```
//!
//! ```should_panic(expected = "doctest panic")
//! panic!("doctest panic");
//! ```
//!
//! ```no_run
//! let value: usize = cless_doctest_contract::library_value().len();
//! assert_eq!(value, 13);
//! ```
//!
//! ```compile_fail,E0308
//! let _: u32 = "not an integer";
//! ```
//!
//! ```ignore
//! panic!("ignored doctest must not run by default");
//! ```

pub fn library_value() -> &'static str {
    env!("CLESS_DOCTEST_BUILD")
}

#[cfg(cless_doctest_cfg)]
pub fn build_cfg_present() -> bool {
    true
}

#[cfg(not(cless_doctest_cfg))]
pub fn build_cfg_present() -> bool {
    false
}
