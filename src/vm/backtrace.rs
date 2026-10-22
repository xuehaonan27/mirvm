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

use super::ctx::Ctx;
use super::dispatch::call_fn_addr;
use super::ir::Module;
use crate::os::unwind;

/// The symbol carriers of one Engine: the object the process symbolizer reads, plus the
/// instruction-pointer token it published for each FuncId.
///
/// This is process state, so it belongs to the Engine that loaded the artifact and not to the
/// artifact itself.
#[derive(Debug, Default)]
pub struct Symbols {
    ips: Vec<u64>,
    /// Owned for its `Drop`: the published tokens stay resolvable only while the object is loaded.
    #[allow(dead_code)]
    image: Option<crate::os::dll::PrivateImage>,
}

/// The stride between published tokens: the writers in `src/native/` lay one slot per function, so
/// an index times this is what follows the offset they return.
const SLOT: usize = crate::arch::asmstub::INERT_SLOT.len();

/// Conservative fallback IP when no symbol image is available. The base comes from this pair's
/// fixed-address layout, which guarantees the band is one no mapping can occupy; a normally loaded
/// Engine publishes a symbolizable address instead.
fn fallback_func_ip(func: u32) -> u64 {
    crate::os_arch::addrspace::FUNC_IP_BASE + (func as u64) * 64
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
    let shared = unsafe { (*ctx).shared };
    // Complete publication before invoking guest callbacks, which may capture another backtrace.
    // Captured IPs stay valid until the Engine releases its symbol image.
    let symbols = shared
        .backtrace_symbols()
        .unwrap_or_else(|error| super::unwind::engine_abort(&error.to_string()));
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
    // Return PCs must be inside the symbol: resolvers subtract one byte to find the call site.
    let mut frames: Vec<GuestUnwindContext> = unsafe {
        (*ctx)
            .shadow
            .iter()
            .map(|frame| GuestUnwindContext {
                ip: symbols.ip_of(frame.func) + 1,
                cfa: frame.cfa,
            })
            .collect()
    };
    frames.extend(host.into_iter().filter_map(|frame| {
        shared
            .jit
            .guest_func_at(frame.ip.saturating_sub(1))
            .map(|func| GuestUnwindContext {
                ip: symbols.ip_of(func) + 1,
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
    pub(crate) fn materialize(module: &Module) -> Result<Self, crate::error::Error> {
        if module.function_names.is_empty() {
            return Ok(Self::default());
        }
        let (bytes, text_off) = crate::native::symbol::image::build(
            crate::os::dll::OBJECT_FORMAT,
            &module.function_names.iter().collect::<Vec<_>>(),
        )?;
        let image = crate::os::dll::load_private_image(&bytes, c"mirvm-guest-symbols").map_err(
            |error| {
                crate::fail!(
                    Engine,
                    format!("publishing the guest symbol object failed: {error}")
                )
            },
        )?;
        let bias = image.bias();
        let ips = (0..module.function_names.len())
            .map(|index| (bias + text_off + index * SLOT) as u64)
            .collect();
        Ok(Self {
            ips,
            image: Some(image),
        })
    }

    /// The token for `func`, or the synthetic fallback when the image published none.
    pub(crate) fn ip_of(&self, func: u32) -> u64 {
        self.ips
            .get(func as usize)
            .copied()
            .unwrap_or_else(|| fallback_func_ip(func))
    }

    pub(crate) fn enclosing_ip(&self, ip: u64) -> Option<u64> {
        let offset = ip.checked_sub(*self.ips.first()?)?;
        self.ips.get(offset as usize / SLOT).copied()
    }

    #[cfg(test)]
    fn image_handle(&self) -> usize {
        self.image.as_ref().unwrap().handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ctx::{Engine, Shared};
    use crate::vm::ir::{Block, FuncBody, RetAbi, Terminator};

    fn named_module(name: &str) -> Module {
        let mut module = Module {
            funcs: vec![FuncBody {
                shared: None,
                prepared: Default::default(),
                objects: Vec::new(),
                frame_size: 0,
                frame_align: 1,
                ret: RetAbi::Zst,
                params: Vec::new(),
                caller_loc_off: None,
                blocks: vec![Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                }],
                name: name.into(),
            }]
            .into(),
            ..Module::default()
        };
        module.exports.insert(name.into(), 0);
        module
    }

    #[test]
    fn ordinary_execution_leaves_symbol_publication_uninitialized() {
        let engine = Engine::new(Shared::new(named_module("no_backtrace")));
        assert!(engine.shared().symbols.get().is_none());
        unsafe { crate::vm::interp::run_export(&engine, "no_backtrace", &[]) }.unwrap();
        assert!(engine.shared().symbols.get().is_none());
        engine.close();
    }

    #[test]
    fn concurrent_captures_share_stable_symbol_addresses() {
        let shared = Shared::new(named_module("mirvm_concurrent_backtrace"));
        assert!(shared.symbols.get().is_none());
        let ready = std::sync::Barrier::new(4);
        let addresses = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        ready.wait();
                        shared.backtrace_symbols().unwrap().ip_of(0)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(addresses[0], fallback_func_ip(0));
        assert!(addresses.iter().all(|address| *address == addresses[0]));
        let symbols = shared.backtrace_symbols().unwrap();
        assert_eq!(symbols.ip_of(0), addresses[0]);
        assert_eq!(
            crate::os::dll::sym(symbols.image_handle(), c"mirvm_concurrent_backtrace"),
            addresses[0] as usize
        );
    }

    #[test]
    fn failed_publication_is_retained_for_subsequent_captures() {
        let shared = Shared::new(named_module("invalid\0name"));
        let first = shared.backtrace_symbols().unwrap_err();
        let second = shared.backtrace_symbols().unwrap_err();
        assert!(std::ptr::eq(first, second));
        assert!(first.to_string().contains("NUL"));
    }

    #[test]
    fn in_memory_elf_exports_function_names_at_indexed_addresses() {
        let module = Module {
            function_names: vec![
                "mirvm_backtrace_probe_a".into(),
                "mirvm_backtrace_probe_b".into(),
            ]
            .into(),
            ..Module::default()
        };
        let symbols = Symbols::materialize(&module).unwrap();
        for (index, expected) in ["mirvm_backtrace_probe_a", "mirvm_backtrace_probe_b"]
            .iter()
            .enumerate()
        {
            let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
            let ip = symbols.ip_of(index as u32);
            assert_eq!(symbols.enclosing_ip(ip + 1), Some(ip));
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
        assert_eq!(symbols.enclosing_ip(0), None);
        assert_eq!(symbols.enclosing_ip(symbols.ip_of(1) + SLOT as u64), None);
    }
}
