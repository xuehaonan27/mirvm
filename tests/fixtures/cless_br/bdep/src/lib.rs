include!(concat!(env!("OUT_DIR"), "/gen.rs"));

#[cfg(bdep_feat)]
pub const GATED: u32 = 42;
