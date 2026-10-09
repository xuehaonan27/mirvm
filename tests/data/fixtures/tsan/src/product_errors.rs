//! The product modules `src/error.rs` converts from, as the smallest types that satisfy those
//! conversions.
//!
//! The failure root every layer returns through has one `From` per module error, including modules
//! that are rustc- or command-line-facing and therefore outside what this harness compiles. Only the
//! conversion has to exist here: no engine test constructs one of these, and a value that is never
//! built cannot hide an engine failure. The code names them as harness stubs so nobody reads one as
//! a real product identity.

macro_rules! product_error {
    ($module:ident, $component:ident) => {
        pub(crate) mod $module {
            /// A product failure this harness cannot produce.
            #[derive(Debug, thiserror::Error, serde::Serialize)]
            pub(crate) enum Error {
                #[error("{detail}")]
                Stub { detail: String },
            }

            crate::diag_codes! {
                Error: $component => { Stub => "harness.product-stub" }
            }
        }
    };
}

product_error!(sysroot, Sysroot);
product_error!(pack, Pack);
product_error!(image, Image);
product_error!(cli, Run);
product_error!(cargo_shim, Runner);
