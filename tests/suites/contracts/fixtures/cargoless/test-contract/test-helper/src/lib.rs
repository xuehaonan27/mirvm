#[cfg(feature = "contract")]
pub fn value() -> &'static str {
    "dev-contract"
}

#[cfg(not(feature = "contract"))]
pub fn value() -> &'static str {
    "wrong-feature-set"
}
