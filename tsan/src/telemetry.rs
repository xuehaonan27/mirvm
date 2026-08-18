//! TSan adapter for the product telemetry modules needed by `src/vm`.

#[path = "../../src/telemetry/capture.rs"]
pub(crate) mod capture;
#[cfg(test)]
#[path = "../../src/telemetry/decode.rs"]
pub(crate) mod decode;
#[path = "../../src/telemetry/format.rs"]
pub(crate) mod format;
