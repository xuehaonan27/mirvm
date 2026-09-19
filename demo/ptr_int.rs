// Real-address memory checks: ptr<->int round trip, real alignment, wildcard pointer dereference
fn main() {
    // Read/write a Box raw pointer after a round trip through usize
    let b = Box::new(0xABCDu64);
    let addr = Box::into_raw(b) as usize;
    println!("u64 对齐: {}", addr % align_of::<u64>() == 0);
    let back = addr as *mut u64; // int2ptr (wildcard provenance) -> dereference
    unsafe { *back += 1 };
    println!("往返读写: {:#x}", unsafe { *back });
    drop(unsafe { Box::from_raw(back) });

    // Real alignment for high-alignment allocations (MirvmAllocBytes must be real)
    let v = vec![1u128; 4];
    println!("u128 对齐: {}", v.as_ptr() as usize % align_of::<u128>() == 0);
    #[repr(align(64))]
    struct Cache([u8; 64]);
    let c = Box::new(Cache([7; 64]));
    println!("64 对齐: {}", &*c as *const Cache as usize % 64 == 0);

    // Pointer arithmetic goes through the usize domain
    let arr = [10u32, 20, 30, 40];
    let p0 = arr.as_ptr() as usize;
    let p3 = (p0 + 3 * size_of::<u32>()) as *const u32;
    println!("元素3: {}", unsafe { *p3 });

    // ZST and function pointer addresses are nonzero and distinct
    let f1 = main as fn() as usize;
    let zst = &() as *const () as usize;
    println!("非零: {} {}", f1 != 0, zst != 0);
}
