//! Adapter: the product's telemetry sources, compiled into this crate under the name
//! `telemetry` so the shared `src/vm` code keeps resolving `crate::telemetry::capture`.
//!
//! This module must stay at the crate root and keep that name: `src/vm/interp` and the
//! JIT helpers talk to `crate::telemetry::capture`, and the harness compiles those sources
//! verbatim. The case that exercises the capture path lives in `cases::capture_session`.

#[path = "../../../../../src/telemetry/capture.rs"]
pub(crate) mod capture;
#[path = "../../../../../src/telemetry/capture_session/mod.rs"]
pub(crate) mod capture_session;
#[path = "../../../../../src/telemetry/capture_writer.rs"]
pub(crate) mod capture_writer;
// The reader side is only needed by capture's own tests, which are not compiled here.
#[cfg(test)]
#[path = "../../../../../src/telemetry/decode/mod.rs"]
pub(crate) mod decode;
#[path = "../../../../../src/telemetry/format/mod.rs"]
pub(crate) mod format;
