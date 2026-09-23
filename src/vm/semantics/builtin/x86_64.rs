//! This architecture's own builtin lane.
//!
//! The guest's `core::arch::x86_64` intrinsics arrive as one builtin variant each. The
//! instruction bodies are `crate::arch::intrinsics`'s; what is decided here is which builtin is
//! which and how its arguments map onto the ABI slots.
//!
//! Two shapes, which is why the routing match in the parent names them as two families: an
//! intrinsic whose result is a vector-register image writes it through the call site's
//! `ret_dst` and reports no scalar result, while one whose result is a scalar returns it.

use crate::vm::ir::Builtin;
use crate::vm::unwind::engine_abort;

/// Executes an intrinsic that writes its result through `ret_dst`.
pub(super) fn exec_indirect_vector(builtin: &Builtin, av: &[u64], ret_dst: Option<u64>) -> u64 {
    let a = |i: usize| av[i];
    match builtin {
        Builtin::X86Pshufb128 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb128 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::intrinsics::pshufb128(dst, a(0) as *const u8, a(1) as *const u8)
            };
            0
        }
        Builtin::X86Pshufb256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb256 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::intrinsics::pshufb256(dst, a(0) as *const u8, a(1) as *const u8)
            };
            0
        }
        Builtin::X86Sha256Msg1 | Builtin::X86Sha256Msg2 => {
            let Some(dst) = ret_dst else {
                engine_abort("sha256msg return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                if matches!(builtin, Builtin::X86Sha256Msg1) {
                    crate::arch::intrinsics::sha256msg1(dst, a(0) as *const u8, a(1) as *const u8);
                } else {
                    crate::arch::intrinsics::sha256msg2(dst, a(0) as *const u8, a(1) as *const u8);
                }
            }
            0
        }
        Builtin::X86Sha256Rnds2 => {
            let Some(dst) = ret_dst else {
                engine_abort("sha256rnds2 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::intrinsics::sha256rnds2(
                    dst,
                    a(0) as *const u8,
                    a(1) as *const u8,
                    a(2) as *const u8,
                );
            }
            0
        }
        Builtin::X86PsadBw128 | Builtin::X86PsadBw256 => {
            let Some(dst) = ret_dst else {
                engine_abort("psad.bw return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                if matches!(builtin, Builtin::X86PsadBw128) {
                    crate::arch::intrinsics::psad_bw128(dst, a(0) as *const u8, a(1) as *const u8);
                } else {
                    crate::arch::intrinsics::psad_bw256(dst, a(0) as *const u8, a(1) as *const u8);
                }
            }
            0
        }
        Builtin::X86Pclmulqdq => {
            let Some(dst) = ret_dst else {
                engine_abort("pclmulqdq return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::intrinsics::pclmulqdq(dst, a(0) as *const u8, a(1) as *const u8, a(2))
            };
            0
        }
        Builtin::X86AesEnc
        | Builtin::X86AesEncLast
        | Builtin::X86AesDec
        | Builtin::X86AesDecLast => {
            let Some(dst) = ret_dst else {
                engine_abort("aesni return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, k) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86AesEnc => crate::arch::intrinsics::aesenc(dst, x, k),
                    Builtin::X86AesEncLast => crate::arch::intrinsics::aesenclast(dst, x, k),
                    Builtin::X86AesDec => crate::arch::intrinsics::aesdec(dst, x, k),
                    _ => crate::arch::intrinsics::aesdeclast(dst, x, k),
                }
            }
            0
        }
        Builtin::X86AesImc => {
            let Some(dst) = ret_dst else {
                engine_abort("aesimc return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::intrinsics::aesimc(dst, a(0) as *const u8) };
            0
        }
        Builtin::X86AesKeygenAssist => {
            let Some(dst) = ret_dst else {
                engine_abort("aeskeygenassist return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::intrinsics::aeskeygenassist(dst, a(0) as *const u8, a(1)) };
            0
        }
        Builtin::X86Permd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("permd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::intrinsics::permd256(dst, a(0) as *const u8, a(1) as *const u8) };
            0
        }
        Builtin::X86PmaddUbSw128
        | Builtin::X86PmaddUbSw256
        | Builtin::X86PmaddWd128
        | Builtin::X86PmaddWd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pmadd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86PmaddUbSw128 => crate::arch::intrinsics::pmaddubsw128(dst, x, y),
                    Builtin::X86PmaddUbSw256 => crate::arch::intrinsics::pmaddubsw256(dst, x, y),
                    Builtin::X86PmaddWd128 => crate::arch::intrinsics::pmaddwd128(dst, x, y),
                    _ => crate::arch::intrinsics::pmaddwd256(dst, x, y),
                }
            }
            0
        }
        Builtin::X86GatherQPd256 | Builtin::X86GatherDPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("gather.pd.256 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            // (src vec, base scalar pointer, vindex vec, mask vec, scale imm)
            unsafe {
                if matches!(builtin, Builtin::X86GatherQPd256) {
                    crate::arch::intrinsics::gather_q_pd_256(
                        dst,
                        a(0) as *const u8,
                        a(1),
                        a(2) as *const u8,
                        a(3) as *const u8,
                        a(4),
                    );
                } else {
                    crate::arch::intrinsics::gather_d_pd_256(
                        dst,
                        a(0) as *const u8,
                        a(1),
                        a(2) as *const u8,
                        a(3) as *const u8,
                        a(4),
                    );
                }
            }
            0
        }
        Builtin::X86Pmadd52Lo128
        | Builtin::X86Pmadd52Hi128
        | Builtin::X86Pmadd52Lo256
        | Builtin::X86Pmadd52Hi256
        | Builtin::X86Pmadd52Lo512
        | Builtin::X86Pmadd52Hi512 => {
            let Some(dst) = ret_dst else {
                engine_abort("vpmadd52 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, z) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86Pmadd52Lo128 => {
                        crate::arch::intrinsics::vpmadd52::<2, false>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Hi128 => {
                        crate::arch::intrinsics::vpmadd52::<2, true>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Lo256 => {
                        crate::arch::intrinsics::vpmadd52::<4, false>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Hi256 => {
                        crate::arch::intrinsics::vpmadd52::<4, true>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Lo512 => {
                        crate::arch::intrinsics::vpmadd52::<8, false>(dst, x, y, z)
                    }
                    _ => crate::arch::intrinsics::vpmadd52::<8, true>(dst, x, y, z),
                }
            }
            0
        }
        Builtin::X86MaxPs128
        | Builtin::X86MinPs128
        | Builtin::X86MaxPs256
        | Builtin::X86MinPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86MaxPs128 => {
                        crate::arch::intrinsics::maxmin_ps::<4, true>(dst, x, y)
                    }
                    Builtin::X86MinPs128 => {
                        crate::arch::intrinsics::maxmin_ps::<4, false>(dst, x, y)
                    }
                    Builtin::X86MaxPs256 => {
                        crate::arch::intrinsics::maxmin_ps::<8, true>(dst, x, y)
                    }
                    _ => crate::arch::intrinsics::maxmin_ps::<8, false>(dst, x, y),
                }
            }
            0
        }
        Builtin::X86MaxSd | Builtin::X86MinSd => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.sd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86MaxSd) {
                    crate::arch::intrinsics::maxmin_pd::<1, true>(dst, x, y)
                } else {
                    crate::arch::intrinsics::maxmin_pd::<1, false>(dst, x, y)
                }
            }
            0
        }
        Builtin::X86MaxPd128
        | Builtin::X86MinPd128
        | Builtin::X86MaxPd256
        | Builtin::X86MinPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.pd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86MaxPd128 => {
                        crate::arch::intrinsics::maxmin_pd::<2, true>(dst, x, y)
                    }
                    Builtin::X86MinPd128 => {
                        crate::arch::intrinsics::maxmin_pd::<2, false>(dst, x, y)
                    }
                    Builtin::X86MaxPd256 => {
                        crate::arch::intrinsics::maxmin_pd::<4, true>(dst, x, y)
                    }
                    _ => crate::arch::intrinsics::maxmin_pd::<4, false>(dst, x, y),
                }
            }
            0
        }
        Builtin::X86CmpPs128 | Builtin::X86CmpPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
            unsafe {
                if matches!(builtin, Builtin::X86CmpPs128) {
                    crate::arch::intrinsics::cmp_ps::<4>(dst, x, y, imm)
                } else {
                    crate::arch::intrinsics::cmp_ps::<8>(dst, x, y, imm)
                }
            }
            0
        }
        Builtin::X86CmpPd128 | Builtin::X86CmpPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.pd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
            unsafe {
                if matches!(builtin, Builtin::X86CmpPd128) {
                    crate::arch::intrinsics::cmp_pd::<2>(dst, x, y, imm)
                } else {
                    crate::arch::intrinsics::cmp_pd::<4>(dst, x, y, imm)
                }
            }
            0
        }
        Builtin::X86RoundPs128 | Builtin::X86RoundPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("round.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, imm) = (a(0) as *const u8, a(1));
            unsafe {
                if matches!(builtin, Builtin::X86RoundPs128) {
                    crate::arch::intrinsics::round_ps::<4>(dst, x, imm)
                } else {
                    crate::arch::intrinsics::round_ps::<8>(dst, x, imm)
                }
            }
            0
        }
        Builtin::X86CvtPs2dq128
        | Builtin::X86CvttPs2dq128
        | Builtin::X86CvtPs2dq256
        | Builtin::X86CvttPs2dq256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cvt(t).ps2dq return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let x = a(0) as *const u8;
            unsafe {
                match builtin {
                    Builtin::X86CvtPs2dq128 => {
                        crate::arch::intrinsics::cvt_ps2dq::<4, false>(dst, x)
                    }
                    Builtin::X86CvttPs2dq128 => {
                        crate::arch::intrinsics::cvt_ps2dq::<4, true>(dst, x)
                    }
                    Builtin::X86CvtPs2dq256 => {
                        crate::arch::intrinsics::cvt_ps2dq::<8, false>(dst, x)
                    }
                    _ => crate::arch::intrinsics::cvt_ps2dq::<8, true>(dst, x),
                }
            }
            0
        }
        Builtin::X86BlendvPs128 | Builtin::X86BlendvPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("blendv.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, m) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86BlendvPs128) {
                    crate::arch::intrinsics::blendv_ps::<4>(dst, x, y, m)
                } else {
                    crate::arch::intrinsics::blendv_ps::<8>(dst, x, y, m)
                }
            }
            0
        }
        Builtin::X86Lddqu128 | Builtin::X86Lddqu256 => {
            let Some(dst) = ret_dst else {
                engine_abort("lddqu return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let src = a(0) as *const u8;
            unsafe {
                if matches!(builtin, Builtin::X86Lddqu128) {
                    crate::arch::intrinsics::lddqu::<16>(dst, src)
                } else {
                    crate::arch::intrinsics::lddqu::<32>(dst, src)
                }
            }
            0
        }
        Builtin::X86Cvtps2ph128 | Builtin::X86Cvtps2ph256 => {
            let Some(dst) = ret_dst else {
                engine_abort("vcvtps2ph return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, imm) = (a(0) as *const u8, a(1));
            unsafe {
                if matches!(builtin, Builtin::X86Cvtps2ph128) {
                    crate::arch::intrinsics::cvtps2ph::<4>(dst, x, imm)
                } else {
                    crate::arch::intrinsics::cvtps2ph::<8>(dst, x, imm)
                }
            }
            0
        }
        Builtin::X86Cvtph2ps128 | Builtin::X86Cvtph2ps256 => {
            let Some(dst) = ret_dst else {
                engine_abort("vcvtph2ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let x = a(0) as *const u8;
            unsafe {
                if matches!(builtin, Builtin::X86Cvtph2ps128) {
                    crate::arch::intrinsics::cvtph2ps::<4>(dst, x)
                } else {
                    crate::arch::intrinsics::cvtph2ps::<8>(dst, x)
                }
            }
            0
        }
        Builtin::X86PsllD128 | Builtin::X86PsrlD128 => {
            let Some(dst) = ret_dst else {
                engine_abort("ps{l,r}l.d return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, c) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86PsllD128) {
                    crate::arch::intrinsics::pshift32::<4, true>(dst, x, c)
                } else {
                    crate::arch::intrinsics::pshift32::<4, false>(dst, x, c)
                }
            }
            0
        }
        _ => engine_abort("non-vector builtin reached the indirect-vector lane"),
    }
}

/// Executes an intrinsic whose result is a scalar.
pub(super) fn exec_scalar(builtin: &Builtin, av: &[u64]) -> u64 {
    let a = |i: usize| av[i];
    match builtin {
        Builtin::Xgetbv => unsafe { crate::arch::intrinsics::xgetbv(a(0) as u32) },
        Builtin::X86Crc32U8 => unsafe {
            u64::from(crate::arch::intrinsics::crc32_u8(a(0) as u32, a(1) as u8))
        },
        Builtin::X86Crc32U16 => unsafe {
            u64::from(crate::arch::intrinsics::crc32_u16(a(0) as u32, a(1) as u16))
        },
        Builtin::X86Crc32U32 => unsafe {
            u64::from(crate::arch::intrinsics::crc32_u32(a(0) as u32, a(1) as u32))
        },
        Builtin::X86Crc32U64 => unsafe { crate::arch::intrinsics::crc32_u64(a(0), a(1)) },
        _ => engine_abort("non-scalar builtin reached the x86_64 scalar lane"),
    }
}
