//! The guest backtrace: the symbol carriers that make an interpreted frame resolvable, and the
//! walk that merges interpreted and JIT frames into one sequence.
//!
//! The standard Rust symbolizer discovers loaded ELF objects with `dl_iterate_phdr` and reads
//! their symbol tables itself, so a symbol is exposed as an ordinary ELF function symbol: one
//! inert byte per FuncId whose address is the opaque instruction-pointer token a frame reports.
//! Nothing ever calls those bytes.
//!
//! A frame sequence is therefore two sources merged by host stack position: the interpreter's
//! shadow frames and the JIT's real machine frames, which the system unwinder reads back.

use std::io::{Seek, Write};
use std::os::fd::FromRawFd;

use super::ctx::Ctx;
use super::dispatch::call_fn_addr;
use super::ir::Module;
use crate::os::unwind;

/// The symbol carriers of one Engine: the in-memory ELF the process symbolizer reads, plus the
/// instruction-pointer token it published for each FuncId.
///
/// This is process state, so it belongs to the Engine that loaded the artifact and not to the
/// artifact itself.
#[derive(Debug, Default)]
pub struct Symbols {
    ips: Vec<u64>,
    /// Owned for its `Drop`: the published tokens stay resolvable only while the object is loaded.
    #[allow(dead_code)]
    image: Option<SymbolImage>,
}

#[derive(Debug)]
struct SymbolImage {
    _file: std::fs::File,
    handle: usize,
}

impl Drop for SymbolImage {
    fn drop(&mut self) {
        unsafe { crate::os::dll::close(self.handle) };
    }
}

/// Conservative fallback IP when no symbol image is available. The base comes from this pair's
/// fixed-address layout, which guarantees the band is one no mapping can occupy; a normally loaded
/// Engine publishes a symbolizable address instead.
fn fallback_func_ip(func: u32) -> u64 {
    crate::os_arch::addrspace::FUNC_IP_BASE + (func as u64) * 64
}

/// The instruction-pointer token for `func`: the address the symbol image published, when there
/// is one.
pub(crate) fn func_synth_ip(ctx: *mut Ctx, func: u32) -> u64 {
    unsafe { &*(*ctx).shared }.symbols.ip_of(func)
}

#[repr(C)]
struct GuestUnwindContext {
    ip: u64,
    cfa: u64,
}

#[repr(C)]
struct HostFrame {
    ip: u64,
    cfa: u64,
}

extern "C" fn collect_host_frame(ctx: unwind::Context, arg: unwind::Context) -> i32 {
    let frames = unsafe { &mut *(arg as *mut Vec<HostFrame>) };
    frames.push(HostFrame {
        ip: unsafe { unwind::frame_ip(ctx) } as u64,
        cfa: unsafe { unwind::frame_cfa(ctx) } as u64,
    });
    0
}

/// `_Unwind_Backtrace(trace_fn, arg)`: the system unwinder reads the live JIT machine frames,
/// which are then merged with the interpreter's shadow frames by host stack position. The
/// callback only ever sees a controlled guest context, never engine host frames.
pub(crate) fn unwind_backtrace(ctx: *mut Ctx, trace_fn: u64, arg: u64) -> u64 {
    let shared = unsafe { &*(*ctx).shared };
    let mut host: Vec<HostFrame> = Vec::new();
    unsafe {
        unwind::backtrace(
            collect_host_frame,
            &mut host as *mut Vec<HostFrame> as unwind::Context,
        );
    }

    // The callback may re-enter the guest and change the live stack, so snapshot everything
    // first. The x86_64 stack grows down, so a smaller CFA is an inner frame; a JIT guest call
    // registers only its fast body, so wrapper frames never appear twice.
    let mut frames: Vec<GuestUnwindContext> = unsafe {
        (*ctx)
            .shadow
            .iter()
            .map(|frame| GuestUnwindContext {
                ip: frame.ip,
                cfa: frame.cfa,
            })
            .collect()
    };
    frames.extend(host.into_iter().filter_map(|frame| {
        shared
            .jit
            .guest_func_at(frame.ip.saturating_sub(1))
            .map(|func| GuestUnwindContext {
                ip: func_synth_ip(ctx, func),
                cfa: frame.cfa,
            })
    }));
    frames.sort_unstable_by_key(|frame| frame.cfa);

    // The standard implementation trims the guest frame that is currently calling
    // _Unwind_Backtrace. Our synthetic symbol address cannot be compared directly with its entry
    // pointer, so skip the innermost guest frame here instead.
    for frame in frames.into_iter().skip(1) {
        let frame_ptr = &frame as *const GuestUnwindContext as u64;
        let r = call_fn_addr(ctx, trace_fn, &[frame_ptr, arg], "_Unwind_Backtrace").0;
        if r != 0 {
            break; // _URC_FOREIGN_EXCEPTION_CAUGHT / _URC_FAILURE and friends: stop
        }
    }
    5 // _URC_END_OF_STACK
}

impl Symbols {
    /// Publishes one symbol per guest function so the process symbolizer can name an interpreted
    /// frame. An artifact without function names gets no image, and a frame falls back to its
    /// synthetic token.
    pub(crate) fn materialize(module: &Module) -> Result<Self, String> {
        if module.function_names.is_empty() {
            return Ok(Self::default());
        }
        let (bytes, text_off) = crate::native::symimage::build(&module.function_names)?;
        let Some(fd) = crate::os::mem::anonymous_file(c"mirvm-guest-symbols") else {
            return Err(format!(
                "memfd_create failed: {}",
                std::io::Error::last_os_error()
            ));
        };
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(&bytes)
            .map_err(|error| format!("writing in-memory ELF failed: {error}"))?;
        file.rewind()
            .map_err(|error| format!("rewinding in-memory ELF failed: {error}"))?;
        let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}"))
            .map_err(|_| "in-memory ELF fd path unexpectedly contains NUL".to_string())?;
        let handle = crate::os::dll::open_with_flags(
            &path,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .map_err(|error| format!("dlopen of in-memory ELF failed: {error}"))?;
        let bias =
            crate::os::dll::load_bias(handle).ok_or("reading in-memory ELF load base failed")?;
        let ips = (0..module.function_names.len())
            .map(|index| (bias + text_off + index * 16) as u64)
            .collect();
        Ok(Self {
            ips,
            image: Some(SymbolImage {
                _file: file,
                handle,
            }),
        })
    }

    /// The token for `func`, or the synthetic fallback when the image published none.
    pub(crate) fn ip_of(&self, func: u32) -> u64 {
        self.ips
            .get(func as usize)
            .copied()
            .unwrap_or_else(|| fallback_func_ip(func))
    }

    #[cfg(test)]
    fn image_handle(&self) -> usize {
        self.image.as_ref().unwrap().handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_elf_exports_function_names_at_indexed_addresses() {
        let module = Module {
            function_names: vec![
                "mirvm_backtrace_probe_a".into(),
                "mirvm_backtrace_probe_b".into(),
            ],
            ..Module::default()
        };
        let symbols = Symbols::materialize(&module).unwrap();
        for (index, expected) in ["mirvm_backtrace_probe_a", "mirvm_backtrace_probe_b"]
            .iter()
            .enumerate()
        {
            let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
            let ip = symbols.ip_of(index as u32);
            let ok = unsafe { libc::dladdr(ip as *const libc::c_void, info.as_mut_ptr()) };
            assert_ne!(ok, 0);
            let info = unsafe { info.assume_init() };
            assert!(!info.dli_sname.is_null());
            assert_eq!(
                unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }
                    .to_str()
                    .unwrap(),
                *expected
            );
            let symbol = std::ffi::CString::new(*expected).unwrap();
            assert_eq!(
                crate::os::dll::sym(symbols.image_handle(), &symbol),
                symbols.ip_of(index as u32) as usize
            );
        }
    }
}
