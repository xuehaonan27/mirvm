//! E9: `dl_iterate_phdr` calls a guest callback and returns its stop value.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
struct DlPhdrInfoPrefix {
    dlpi_addr: usize,
    dlpi_name: *const c_char,
    dlpi_phdr: *const c_void,
    dlpi_phnum: u16,
}

unsafe extern "C" {
    fn dl_iterate_phdr(
        callback: unsafe extern "C" fn(*mut DlPhdrInfoPrefix, usize, *mut c_void) -> c_int,
        data: *mut c_void,
    ) -> c_int;
}

unsafe extern "C" fn visit(info: *mut DlPhdrInfoPrefix, size: usize, data: *mut c_void) -> c_int {
    let valid = !info.is_null()
        && size >= size_of::<DlPhdrInfoPrefix>()
        && unsafe { !(*info).dlpi_phdr.is_null() && (*info).dlpi_phnum != 0 };
    if valid {
        unsafe { *(data as *mut bool) = true };
        73
    } else {
        0
    }
}

fn main() {
    let mut visited = false;
    let result = unsafe { dl_iterate_phdr(visit, (&mut visited as *mut bool).cast::<c_void>()) };
    println!("dl_iterate_phdr visited={visited} stop={result}");
}
