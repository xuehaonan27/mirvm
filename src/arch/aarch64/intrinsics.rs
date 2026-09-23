//! This CPU's answer to the `llvm.x86.*` instruction boundary: there is nothing behind it.
//!
//! The builtin lane in `src/vm/semantics/builtin` names one body per guest intrinsic through the
//! ladder in `super::super`, so every target has the same names to call. This CPU has no such
//! instruction, and a builtin variant is only ever produced by lowering the guest it describes:
//! this build's guest is not x86_64, so nothing here is reachable.
//!
//! The signatures are x86_64's, which is what keeps the two halves of the ladder interchangeable.
//! A body added there without one here is an unresolved name on this target, so the compiler is
//! what holds the two lists together. The generic parameters sit in brackets of their own only so
//! that the macro below can tell them from the argument list; they are emitted verbatim.

/// One unreachable body per signature, so the refusal reads the same wherever it is written.
macro_rules! no_such_instruction {
    ($( fn $name:ident [ $($generics:tt)* ] ( $($args:tt)* ) $( -> $ret:ty)? ; )*) => {$(
        #[allow(unused_variables)]
        pub(crate) unsafe fn $name $($generics)* ( $($args)* ) $(-> $ret)? {
            unreachable!(concat!("this CPU has no `", stringify!($name), "` instruction"))
        }
    )*};
}

no_such_instruction! {
    fn xgetbv [] (xcr: u32) -> u64;
    fn pshufb128 [] (dst: *mut u8, a: *const u8, control: *const u8) ;
    fn pshufb256 [] (dst: *mut u8, a: *const u8, control: *const u8) ;
    fn sha256msg1 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn sha256msg2 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn sha256rnds2 [] (dst: *mut u8, a: *const u8, b: *const u8, round_keys: *const u8) ;
    fn psad_bw128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn psad_bw256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pclmulqdq [] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64) ;
    fn aesenc [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesenclast [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdec [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdeclast [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesimc [] (dst: *mut u8, a: *const u8) ;
    fn aeskeygenassist [] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn crc32_u8 [] (crc: u32, v: u8) -> u32;
    fn crc32_u16 [] (crc: u32, v: u16) -> u32;
    fn crc32_u32 [] (crc: u32, v: u32) -> u32;
    fn crc32_u64 [] (crc: u64, v: u64) -> u64;
    fn permd256 [] (dst: *mut u8, a: *const u8, idx: *const u8) ;
    fn pmaddubsw128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pmaddubsw256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pmaddwd128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn lddqu [<const W: usize>] (dst: *mut u8, src: *const u8) ;
    fn pmaddwd256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn gather_q_pd_256 [] (dst: *mut u8, src: *const u8, base: u64, vindex: *const u8, mask: *const u8, scale: u64,) ;
    fn gather_d_pd_256 [] (dst: *mut u8, src: *const u8, base: u64, vindex: *const u8, mask: *const u8, scale: u64,) ;
    fn vpmadd52 [<const LANES: usize, const HI: bool>] (dst: *mut u8, a: *const u8, b: *const u8, c: *const u8,) ;
    fn maxmin_ps [<const LANES: usize, const MAX: bool>] (dst: *mut u8, a: *const u8, b: *const u8,) ;
    fn maxmin_pd [<const LANES: usize, const MAX: bool>] (dst: *mut u8, a: *const u8, b: *const u8,) ;
    fn cmp_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64,) ;
    fn cmp_pd [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64,) ;
    fn round_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn cvt_ps2dq [<const LANES: usize, const TRUNC: bool>] (dst: *mut u8, a: *const u8) ;
    fn blendv_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, mask: *const u8,) ;
    fn pshift32 [<const LANES: usize, const LEFT: bool>] (dst: *mut u8, a: *const u8, count: *const u8,) ;
    fn cvtps2ph [<const LANES: usize>] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn cvtph2ps [<const LANES: usize>] (dst: *mut u8, a: *const u8) ;
}
