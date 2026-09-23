//! When a foreign object's constructors and destructors run.
//!
//! ELF hands an object two arrays of function pointers -- `.init_array` and `.fini_array`, plus
//! the singular `DT_INIT`/`DT_FINI` -- and the C runtime runs the first before any code in the
//! object and the second at teardown. An image is mapped once for the process, but its language
//! lifecycle belongs to the Engine that loaded it, so this module owns both halves: reading the
//! addresses out of a loaded object's dynamic table and taking the system loader's hands off
//! them, and the state machine that runs each array at most once per instance.
//!
//! How the object got mapped is not this module's business: `native_instance` goes through
//! `dlopen` and `mcload` maps the bytes itself, and both hand the tags they read to
//! [`DynamicLifecycle::materialize`].

use std::ffi::{CString, c_char, c_int};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::native::elf;

/// Constructor and destructor addresses after relocation. Images deliberately
/// remain mapped for the process, but their language lifecycle still runs once
/// per Engine instance.
#[derive(Debug, Default)]
pub(crate) struct NativeLifecycle {
    initializers: Box<[usize]>,
    finalizers: Box<[usize]>,
    state: AtomicU8,
}

const LIFECYCLE_UNSTARTED: u8 = 0;
const LIFECYCLE_STARTING: u8 = 1;
const LIFECYCLE_COMPLETED: u8 = 2;
const LIFECYCLE_FINALIZED: u8 = 3;

pub(crate) struct InitializerArgs {
    _argv_storage: Vec<CString>,
    _env_storage: Vec<CString>,
    argv: Vec<*mut c_char>,
    envp: Vec<*mut c_char>,
}

impl InitializerArgs {
    pub(crate) fn capture() -> Result<Self, String> {
        let argv_storage = std::env::args_os()
            .map(|value| {
                CString::new(crate::os::fs::raw_bytes(value.as_os_str()))
                    .map_err(|_| "process argument contains NUL".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let env_storage = std::env::vars_os()
            .map(|(key, value)| {
                let mut bytes = crate::os::fs::raw_bytes(key.as_os_str()).to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(crate::os::fs::raw_bytes(value.as_os_str()));
                CString::new(bytes).map_err(|_| "process environment contains NUL".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut argv = argv_storage
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let mut envp = env_storage
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        argv.push(std::ptr::null_mut());
        envp.push(std::ptr::null_mut());
        Ok(Self {
            _argv_storage: argv_storage,
            _env_storage: env_storage,
            argv,
            envp,
        })
    }
}

impl NativeLifecycle {
    pub(crate) fn new(initializers: Vec<usize>, finalizers: Vec<usize>) -> Self {
        Self {
            initializers: initializers.into_boxed_slice(),
            finalizers: finalizers.into_boxed_slice(),
            state: AtomicU8::new(LIFECYCLE_UNSTARTED),
        }
    }

    pub(crate) fn run_initializers(&self, args: &mut InitializerArgs) {
        if self
            .state
            .compare_exchange(
                LIFECYCLE_UNSTARTED,
                LIFECYCLE_STARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        for &address in &self.initializers {
            let init: unsafe extern "C-unwind" fn(c_int, *mut *mut c_char, *mut *mut c_char) =
                unsafe { std::mem::transmute(address) };
            unsafe {
                init(
                    (args.argv.len() - 1) as c_int,
                    args.argv.as_mut_ptr(),
                    args.envp.as_mut_ptr(),
                )
            };
        }
        self.state.store(LIFECYCLE_COMPLETED, Ordering::Release);
    }

    pub(crate) fn run_finalizers(&self) {
        if self
            .state
            .compare_exchange(
                LIFECYCLE_COMPLETED,
                LIFECYCLE_FINALIZED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        for &address in &self.finalizers {
            let fini: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(address) };
            unsafe { fini() };
        }
    }
}

#[derive(Default)]
pub(crate) struct DynamicLifecycle {
    init: Option<u64>,
    init_array: Option<(u64, u64)>,
    fini: Option<u64>,
    fini_array: Option<(u64, u64)>,
    loads: Vec<(u64, u64)>,
    executable_loads: Vec<(u64, u64)>,
}

impl DynamicLifecycle {
    pub(crate) fn executable_ranges(&self, bias: usize) -> Result<Box<[(usize, usize)]>, String> {
        self.executable_loads
            .iter()
            .map(|&(start, end)| {
                let start = add_bias(bias, start, "executable PT_LOAD")?;
                let end = add_bias(bias, end, "executable PT_LOAD")?;
                Ok((start, end))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Vec::into_boxed_slice)
    }

    pub(crate) fn materialize(self, bias: usize) -> Result<NativeLifecycle, String> {
        let mut initializers = Vec::new();
        if let Some(init) = self.init {
            initializers.push(add_bias(bias, init, "DT_INIT")?);
        }
        if let Some((address, size)) = self.init_array {
            initializers.extend(read_function_array(
                bias,
                address,
                size,
                &self.loads,
                "DT_INIT_ARRAY",
            )?);
        }

        let mut finalizers = Vec::new();
        if let Some((address, size)) = self.fini_array {
            let mut array = read_function_array(bias, address, size, &self.loads, "DT_FINI_ARRAY")?;
            array.reverse();
            finalizers.extend(array);
        }
        if let Some(fini) = self.fini {
            finalizers.push(add_bias(bias, fini, "DT_FINI")?);
        }
        Ok(NativeLifecycle::new(initializers, finalizers))
    }
}

fn add_bias(bias: usize, value: u64, what: &str) -> Result<usize, String> {
    bias.checked_add(
        usize::try_from(value).map_err(|_| format!("{what} address does not fit usize"))?,
    )
    .ok_or_else(|| format!("{what} address overflow"))
}

fn read_function_array(
    bias: usize,
    address: u64,
    size: u64,
    loads: &[(u64, u64)],
    what: &str,
) -> Result<Vec<usize>, String> {
    if !size.is_multiple_of(8) || size > 1 << 20 {
        return Err(format!("{what} has invalid size {size}"));
    }
    let end = address
        .checked_add(size)
        .ok_or_else(|| format!("{what} range overflow"))?;
    if !loads
        .iter()
        .any(|&(start, load_end)| address >= start && end <= load_end)
    {
        return Err(format!("{what} lies outside a loadable segment"));
    }
    let start = add_bias(bias, address, what)?;
    let mut functions = Vec::with_capacity((size / 8) as usize);
    for index in 0..(size / 8) as usize {
        let value = unsafe { (start as *const usize).add(index).read_unaligned() };
        if value != 0 && value != usize::MAX {
            functions.push(value);
        }
    }
    Ok(functions)
}

/// Remove dynamic-loader ownership of init/fini from this unique private copy.
/// Mapping, dependency resolution and relocations still use dlopen; the saved
/// addresses are invoked explicitly only after the Engine patches P1 slots.
pub(crate) fn suppress_lifecycle_tags(path: &Path) -> Result<DynamicLifecycle, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|e| format!("fail to read private native `{}`: {e}", path.display()))?;
    let bad = || format!("native `{}` is not valid ELF64 LE", path.display());
    let header = elf::FileHeader::parse(&bytes).ok_or_else(bad)?;
    if header.kind != elf::ET_DYN || header.machine != crate::arch::ELF_MACHINE {
        return Err(bad());
    }
    let phoff = usize::try_from(header.phoff).map_err(|_| bad())?;
    let phentsize = usize::from(header.phentsize);
    let phnum = usize::from(header.phnum);
    if phentsize < elf::PHDR_SIZE {
        return Err(bad());
    }
    let mut dynamic = None;
    let mut result = DynamicLifecycle::default();
    for index in 0..phnum {
        let base = phoff
            .checked_add(index.checked_mul(phentsize).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let ty = elf::u32_at(&bytes, base + elf::phdr::TYPE).ok_or_else(bad)?;
        let flags = elf::u32_at(&bytes, base + elf::phdr::FLAGS).ok_or_else(bad)?;
        let off = elf::u64_at(&bytes, base + elf::phdr::OFFSET).ok_or_else(bad)?;
        let vaddr = elf::u64_at(&bytes, base + elf::phdr::VADDR).ok_or_else(bad)?;
        let filesz = elf::u64_at(&bytes, base + elf::phdr::FILESZ).ok_or_else(bad)?;
        let memsz = elf::u64_at(&bytes, base + elf::phdr::MEMSZ).ok_or_else(bad)?;
        if ty == elf::PT_LOAD {
            let end = vaddr
                .checked_add(memsz)
                .ok_or_else(|| format!("native `{}` load range overflow", path.display()))?;
            result.loads.push((vaddr, end));
            if flags & elf::PF_X != 0 {
                result.executable_loads.push((vaddr, end));
            }
        } else if ty == elf::PT_DYNAMIC {
            dynamic = Some((off, filesz));
        }
    }
    let Some((dynamic_off, dynamic_size)) = dynamic else {
        return Err(format!("native `{}` has no PT_DYNAMIC", path.display()));
    };
    let start = usize::try_from(dynamic_off).map_err(|_| bad())?;
    let size = usize::try_from(dynamic_size).map_err(|_| bad())?;
    let end = start.checked_add(size).ok_or_else(bad)?;
    if end > bytes.len() || !size.is_multiple_of(elf::DYN_ENTRY_SIZE) {
        return Err(bad());
    }

    let mut init_array_addr = None;
    let mut init_array_size = None;
    let mut fini_array_addr = None;
    let mut fini_array_size = None;
    for entry in (start..end).step_by(elf::DYN_ENTRY_SIZE) {
        let tag = i64::from_le_bytes(bytes[entry..entry + 8].try_into().map_err(|_| bad())?);
        if tag == elf::DT_NULL {
            break;
        }
        let value = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().map_err(|_| bad())?);
        match tag {
            elf::DT_INIT => result.init = Some(value),
            elf::DT_FINI => result.fini = Some(value),
            elf::DT_INIT_ARRAY => init_array_addr = Some(value),
            elf::DT_INIT_ARRAYSZ => init_array_size = Some(value),
            elf::DT_FINI_ARRAY => fini_array_addr = Some(value),
            elf::DT_FINI_ARRAYSZ => fini_array_size = Some(value),
            elf::DT_PREINIT_ARRAY | elf::DT_PREINIT_ARRAYSZ => {
                return Err(format!(
                    "native `{}` unexpectedly contains a preinit array",
                    path.display()
                ));
            }
            _ => continue,
        }
        // DT_BIND_NOW is a harmless boolean tag under the already requested
        // RTLD_NOW mode. Replacing lifecycle tags removes them from l_info
        // without terminating the dynamic table early or changing relocation.
        bytes[entry..entry + 8].copy_from_slice(&elf::DT_BIND_NOW.to_le_bytes());
        bytes[entry + 8..entry + 16].fill(0);
    }
    result.init_array = pair_tags(init_array_addr, init_array_size, "DT_INIT_ARRAY")?;
    result.fini_array = pair_tags(fini_array_addr, fini_array_size, "DT_FINI_ARRAY")?;
    std::fs::write(path, bytes).map_err(|e| {
        format!(
            "fail to suppress native lifecycle `{}`: {e}",
            path.display()
        )
    })?;
    Ok(result)
}

fn pair_tags(
    address: Option<u64>,
    size: Option<u64>,
    what: &str,
) -> Result<Option<(u64, u64)>, String> {
    match (address, size) {
        (None, None) => Ok(None),
        (Some(address), Some(size)) => Ok(Some((address, size))),
        _ => Err(format!("native has incomplete {what} metadata")),
    }
}
