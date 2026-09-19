//! LSDA generation (layout copied line for line from the ABI; do not invent).

/// Hand-built GccExceptTable (cleanup-only, no type_info; cg_clif layout, **full
/// coverage**):
/// - call site with no handler: (ret_addr-1, len=1, lpad=0, action=0) -- matching it
///   yields EHAction::None (rust's find_eh_action `cs_lpad == 0` branch)
/// - call site with a cleanup handler: (ret_addr-1, len=1, pad, action=0)
///   rust's find_eh_action returns EHAction::Terminate (= _URC_FATAL) for an ip that is
///   absent from the table, unlike libgcc's __gcc_personality_v0 (no entry = None), so
///   the call-site table must cover every call in the function. This is also why cg_clif
///   emits lpad=0 entries for handler-less sites.
///   Entries follow buffer.call_sites() order (= instruction order), which matches
///   rust's assumption that the table is sorted.
pub(crate) fn build_lsda(call_sites: &[(u64, Option<u64>)]) -> Vec<u8> {
    fn uleb(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    let mut out = vec![0xff, 0xff, 0x01]; // lpStart=omit, ttype=omit, csEncoding=uleb128
    let mut body = Vec::new();
    for &(ret_addr, pad) in call_sites {
        uleb(&mut body, ret_addr - 1);
        uleb(&mut body, 1);
        uleb(&mut body, pad.unwrap_or(0));
        uleb(&mut body, 0); // action=0
    }
    uleb(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out
}

/// Collect every call site after the function was defined; same data source and
/// reading as cg_clif's add_function (no handler -> None, i.e. an lpad=0 entry;
/// cleanup tag -> Some(landing pad address)).
pub(crate) fn collect_call_sites(cctx: &cranelift_codegen::Context) -> Vec<(u64, Option<u64>)> {
    let cc = cctx
        .compiled_code()
        .expect("call sites can only be collected after define");
    let mut cs = Vec::new();
    for site in cc.buffer.call_sites() {
        if site.exception_handlers.is_empty() {
            cs.push((u64::from(site.ret_addr), None));
        }
        for h in site.exception_handlers {
            if let cranelift_codegen::FinalizedMachExceptionHandler::Tag(tag, lp) = h {
                assert_eq!(tag.as_u32(), 0, "this pipeline only emits the cleanup tag");
                cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
            }
        }
    }
    cs
}
