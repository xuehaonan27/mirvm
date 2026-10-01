//! The guest intrinsic names this CPU lowers to a builtin.
//!
//! One line per name, in one table, because the two halves must agree: a name here without a body
//! in [`crate::arch::x86_64::intrinsics`] is a call that resolves and then fails, and a body there
//! without a name is unreachable code. The semantics lane names those bodies through
//! [`crate::arch::intrinsics`], so the compiler is what holds the signatures together and this
//! table is what holds the names.

use rustc_data_structures::fx::FxHashMap;
use rustc_span::Symbol;

use super::builtin_table;
use crate::vm::ir;

/// Register every guest intrinsic name this CPU's lowering can produce.
pub(super) fn register(out: &mut FxHashMap<Symbol, ir::Builtin>) {
    builtin_table!(out;
        "llvm.x86.sse2.pause" => CpuHintNop,
        "llvm.x86.avx.vzeroupper" => CpuHintNop,
        "llvm.x86.addcarry.64" => AddCarry64,
        "llvm.x86.subborrow.64" => SubBorrow64,
        "llvm.x86.xgetbv" => Xgetbv,
        "llvm.x86.ssse3.pshuf.b.128" => X86Pshufb128,
        "llvm.x86.avx2.pshuf.b" => X86Pshufb256,
        "llvm.x86.sha256msg1" => X86Sha256Msg1,
        "llvm.x86.sha256msg2" => X86Sha256Msg2,
        "llvm.x86.sha256rnds2" => X86Sha256Rnds2,
        "llvm.x86.sse2.psad.bw" => X86PsadBw128,
        "llvm.x86.avx2.psad.bw" => X86PsadBw256,
        "llvm.x86.pclmulqdq" => X86Pclmulqdq,
        "llvm.x86.pclmulqdq.256" => X86Pclmulqdq256,
        "llvm.x86.pclmulqdq.512" => X86Pclmulqdq512,
        "llvm.x86.aesni.aesenc" => X86AesEnc,
        "llvm.x86.aesni.aesenclast" => X86AesEncLast,
        "llvm.x86.aesni.aesdec" => X86AesDec,
        "llvm.x86.aesni.aesdeclast" => X86AesDecLast,
        "llvm.x86.aesni.aesimc" => X86AesImc,
        "llvm.x86.aesni.aeskeygenassist" => X86AesKeygenAssist,
        "llvm.x86.sse42.crc32.32.8" => X86Crc32U8,
        "llvm.x86.sse42.crc32.32.16" => X86Crc32U16,
        "llvm.x86.sse42.crc32.32.32" => X86Crc32U32,
        "llvm.x86.sse42.crc32.64.64" => X86Crc32U64,
        "llvm.x86.avx2.permd" => X86Permd256,
        "llvm.x86.avx2.gather.q.pd.256" => X86GatherQPd256,
        "llvm.x86.avx2.gather.d.pd.256" => X86GatherDPd256,
        "llvm.x86.avx512.vpmadd52l.uq.128" => X86Pmadd52Lo128,
        "llvm.x86.avx512.vpmadd52h.uq.128" => X86Pmadd52Hi128,
        "llvm.x86.avx512.vpmadd52l.uq.256" => X86Pmadd52Lo256,
        "llvm.x86.avx512.vpmadd52h.uq.256" => X86Pmadd52Hi256,
        "llvm.x86.avx512.vpmadd52l.uq.512" => X86Pmadd52Lo512,
        "llvm.x86.avx512.vpmadd52h.uq.512" => X86Pmadd52Hi512,
        "llvm.x86.ssse3.pmadd.ub.sw.128" => X86PmaddUbSw128,
        "llvm.x86.avx2.pmadd.ub.sw" => X86PmaddUbSw256,
        "llvm.x86.sse2.pmadd.wd" => X86PmaddWd128,
        "llvm.x86.avx2.pmadd.wd" => X86PmaddWd256,
        // LDDQU: the pure load `c_tantivy`'s bitpacking/termdict column-value read dispatch uses.
        "llvm.x86.sse3.ldu.dq" => X86Lddqu128,
        "llvm.x86.avx.ldu.dq.256" => X86Lddqu256,
        // F16C: reached only after the runtime feature detection selects that path.
        "llvm.x86.vcvtps2ph.128" => X86Cvtps2ph128,
        "llvm.x86.vcvtph2ps.128" => X86Cvtph2ps128,
        "llvm.x86.vcvtps2ph.256" => X86Cvtps2ph256,
        "llvm.x86.vcvtph2ps.256" => X86Cvtph2ps256,
        // Packed f32: tiny-skia's simd default path. `rcp`/`rsqrt` are deliberately not registered --
        // a hardware approximation is not reproducibly portable, so those names stay a loud Trap.
        "llvm.x86.sse.max.ps" => X86MaxPs128,
        "llvm.x86.sse.min.ps" => X86MinPs128,
        "llvm.x86.avx.max.ps.256" => X86MaxPs256,
        "llvm.x86.avx.min.ps.256" => X86MinPs256,
        "llvm.x86.sse.cmp.ps" => X86CmpPs128,
        "llvm.x86.avx.cmp.ps.256" => X86CmpPs256,
        "llvm.x86.sse2.cmp.pd" => X86CmpPd128,
        "llvm.x86.avx.cmp.pd.256" => X86CmpPd256,
        "llvm.x86.sse2.max.pd" => X86MaxPd128,
        "llvm.x86.sse2.min.pd" => X86MinPd128,
        "llvm.x86.avx.max.pd.256" => X86MaxPd256,
        "llvm.x86.avx.min.pd.256" => X86MinPd256,
        "llvm.x86.sse2.max.sd" => X86MaxSd,
        "llvm.x86.sse2.min.sd" => X86MinSd,
        "llvm.x86.sse41.round.ps" => X86RoundPs128,
        "llvm.x86.avx.round.ps.256" => X86RoundPs256,
        "llvm.x86.sse2.cvtps2dq" => X86CvtPs2dq128,
        "llvm.x86.sse2.cvttps2dq" => X86CvttPs2dq128,
        "llvm.x86.avx.cvt.ps2dq.256" => X86CvtPs2dq256,
        "llvm.x86.avx.cvtt.ps2dq.256" => X86CvttPs2dq256,
        "llvm.x86.sse41.blendvps" => X86BlendvPs128,
        "llvm.x86.avx.blendv.ps.256" => X86BlendvPs256,
        "llvm.x86.sse2.psll.d" => X86PsllD128,
        "llvm.x86.sse2.psrl.d" => X86PsrlD128,
    );
}
