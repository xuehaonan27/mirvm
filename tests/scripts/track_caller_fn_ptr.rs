#[track_caller]
#[inline(never)]
fn tracked<T: Copy>(value: &T) -> (T, u32) {
    (*value, std::panic::Location::caller().line())
}

fn main() {
    let function: fn(&u32) -> (u32, u32) = tracked::<u32>;
    println!("tracked={:?}", function(&41));
}
