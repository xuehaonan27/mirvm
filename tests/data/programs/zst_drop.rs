// Regression probe for address-taking on a 0-byte ZST frame in jit_compile analyze_frame (the
// FrameMap force case): when a ZST with a custom Drop is passed by value through mem::drop, the
// glue call's argument is AddrOf(Local(0)) and there is no slot to anchor at fsz=0. The frame
// must still supply frame_ss so the "taken address offset lands in the frame" assertion holds.
struct G;
impl Drop for G {
    fn drop(&mut self) {
        // Empty body: the glue only needs the address of &_1 itself, not any field
    }
}

#[inline(never)]
fn cycle() -> u64 {
    drop(G);
    1
}

fn main() {
    let mut n = 0u64;
    for _ in 0..8 {
        n += cycle();
    }
    println!("n={n}");
}
