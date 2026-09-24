//! When a foreign object's constructors and destructors run.
//!
//! A foreign object hands its platform a list of constructor functions and a list of destructor
//! functions, and the platform's runtime runs the first before any code in the object and the
//! second at teardown. An image is mapped once for the process, but its language lifecycle belongs
//! to the Engine that loaded it, so this module owns both halves: turning the addresses an object
//! declared into callables of this process, and the state machine that runs each list at most once
//! per instance.
//!
//! Where those addresses are in the object is the object format's, so reading them -- and taking
//! the loader's hands off them -- is [`crate::native::lifecycle`]. What is left here is what the
//! addresses mean once the image is mapped and slid, which the loader does not spell.
//!
//! How the object got mapped is not this module's business either: `native_instance` goes through
//! `dlopen` and `mcload` maps the bytes itself, and both hand what they read to
//! [`materialize`].

use std::ffi::{CString, c_char, c_int};
use std::sync::atomic::{AtomicU8, Ordering};

use crate::native::lifecycle::{CallableList, Layout};

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

/// The instruction ranges of the image `layout` describes, slid to where it was mapped.
pub(crate) fn executable_ranges(
    layout: &Layout,
    bias: usize,
) -> Result<Box<[(usize, usize)]>, String> {
    layout
        .executable_loads
        .iter()
        .map(|&(start, end)| {
            let start = add_bias(bias, start, "executable load")?;
            let end = add_bias(bias, end, "executable load")?;
            Ok((start, end))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Vec::into_boxed_slice)
}

/// The callables `layout` declared, read out of the image now that it is mapped at `bias`.
///
/// A destructor list runs in reverse, which the loader's own order is: the object declared them in
/// construction order and teardown unwinds it.
pub(crate) fn materialize(layout: Layout, bias: usize) -> Result<NativeLifecycle, String> {
    let mut initializers = Vec::new();
    if let Some(init) = layout.init {
        initializers.push(add_bias(bias, init, "the singular constructor")?);
    }
    if let Some((address, size, form)) = layout.init_array {
        initializers.extend(read_callable_list(
            bias,
            address,
            size,
            form,
            &layout.loads,
            "the constructor list",
        )?);
    }

    let mut finalizers = Vec::new();
    if let Some((address, size, form)) = layout.fini_array {
        let mut list = read_callable_list(
            bias,
            address,
            size,
            form,
            &layout.loads,
            "the destructor list",
        )?;
        list.reverse();
        finalizers.extend(list);
    }
    if let Some(fini) = layout.fini {
        finalizers.push(add_bias(bias, fini, "the singular destructor")?);
    }
    Ok(NativeLifecycle::new(initializers, finalizers))
}

fn add_bias(bias: usize, value: u64, what: &str) -> Result<usize, String> {
    bias.checked_add(
        usize::try_from(value).map_err(|_| format!("{what} address does not fit usize"))?,
    )
    .ok_or_else(|| format!("{what} address overflow"))
}

/// The callables a list holds, in the order the image declares them.
///
/// A pointer list holds addresses the loader has already slid, so each entry is the callable. An
/// offset list holds 32-bit distances from the image's base, so each entry becomes a callable by
/// adding the same bias the base was mapped at.
fn read_callable_list(
    bias: usize,
    address: u64,
    size: u64,
    form: CallableList,
    loads: &[(u64, u64)],
    what: &str,
) -> Result<Vec<usize>, String> {
    let stride: u64 = match form {
        CallableList::Pointers => 8,
        CallableList::Offsets => 4,
    };
    if !size.is_multiple_of(stride) || size > 1 << 20 {
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
    let mut functions = Vec::with_capacity((size / stride) as usize);
    for index in 0..(size / stride) as usize {
        let value = match form {
            CallableList::Pointers => unsafe {
                (start as *const usize).add(index).read_unaligned()
            },
            CallableList::Offsets => {
                let offset = unsafe { (start as *const u32).add(index).read_unaligned() as usize };
                add_bias(bias, offset as u64, what)?
            }
        };
        if value != 0 && value != usize::MAX {
            functions.push(value);
        }
    }
    Ok(functions)
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_int};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{InitializerArgs, NativeLifecycle};

    static GOOD_INIT: AtomicU64 = AtomicU64::new(0);
    static BAD_INIT: AtomicU64 = AtomicU64::new(0);
    static GOOD_FINI: AtomicU64 = AtomicU64::new(0);
    static BAD_FINI: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C-unwind" fn good_init(
        argc: c_int,
        argv: *mut *mut c_char,
        envp: *mut *mut c_char,
    ) {
        assert!(argc > 0);
        assert!(!argv.is_null());
        assert!(!envp.is_null());
        GOOD_INIT.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn bad_init(_: c_int, _: *mut *mut c_char, _: *mut *mut c_char) {
        BAD_INIT.fetch_add(1, Ordering::SeqCst);
        panic!("constructor failed after starting");
    }

    unsafe extern "C-unwind" fn good_fini() {
        GOOD_FINI.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn bad_fini() {
        BAD_FINI.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn only_fully_initialized_images_are_finalized() {
        GOOD_INIT.store(0, Ordering::SeqCst);
        BAD_INIT.store(0, Ordering::SeqCst);
        GOOD_FINI.store(0, Ordering::SeqCst);
        BAD_FINI.store(0, Ordering::SeqCst);
        let good = NativeLifecycle::new(
            vec![good_init as *const () as usize],
            vec![good_fini as *const () as usize],
        );
        let bad = NativeLifecycle::new(
            vec![bad_init as *const () as usize],
            vec![bad_fini as *const () as usize],
        );
        let mut args = InitializerArgs::capture().unwrap();

        good.run_initializers(&mut args);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bad.run_initializers(&mut args)
            }))
            .is_err()
        );
        bad.run_finalizers();
        good.run_finalizers();
        good.run_finalizers();

        assert_eq!(GOOD_INIT.load(Ordering::SeqCst), 1);
        assert_eq!(BAD_INIT.load(Ordering::SeqCst), 1);
        assert_eq!(GOOD_FINI.load(Ordering::SeqCst), 1);
        assert_eq!(BAD_FINI.load(Ordering::SeqCst), 0);
    }
}
