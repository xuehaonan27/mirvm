//! Signal lifecycle tests: sigaction old-action visibility, external handler
//! adoption, libc error paths, native-image signal bridges and signal faults
//! crossing an Engine boundary.
//!
//! The siblings below hold one subject each; shared fixtures live here.

mod adoption;
mod bridge;
mod cross_engine;
mod libc_errors;
mod oldact;

use super::*;
