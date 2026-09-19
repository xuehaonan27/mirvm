use std::alloc::{GlobalAlloc, Layout, System};

struct CustomAllocator;

unsafe impl GlobalAlloc for CustomAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CustomAllocator = CustomAllocator;

fn main() {
    let layout = Layout::from_size_align(123, 8).unwrap();
    std::alloc::handle_alloc_error(layout);
}
