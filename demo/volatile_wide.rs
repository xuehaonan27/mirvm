use std::mem::MaybeUninit;

#[repr(C)]
#[derive(Clone, Copy)]
struct Wide {
    marker: u64,
    payload: [u8; 128],
}

fn main() {
    let source = MaybeUninit::new(Wide {
        marker: 0x0123_4567_89ab_cdef,
        payload: std::array::from_fn(|index| index as u8),
    });
    let mut target = MaybeUninit::<Wide>::uninit();

    unsafe {
        std::ptr::write_volatile(&raw mut target, source);
        let value = std::ptr::read_volatile(&raw const target).assume_init();
        let checksum: u32 = value.payload.iter().map(|&byte| u32::from(byte)).sum();
        println!("wide={:#018x} checksum={checksum}", value.marker);
    }
}
