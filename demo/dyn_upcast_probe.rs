//! C5 dyn upcasting probe: when the principal changes, the target vtable =
//! *(source vtable + supertrait_vtable_slot×8). Two forms — Arc wrapper chain
//! (Arc → NonNull → Pat → *const ArcInner → lockstep → data) and direct & reference.
//! See decision-history §7.13.
use std::any::Any;
use std::sync::Arc;

trait Source: Any + Send + Sync {
    fn v(&self) -> u64;
}
struct S(u64);
impl Source for S {
    fn v(&self) -> u64 {
        self.0
    }
}

fn main() {
    // Arc<dyn Source> → Arc<dyn Any + Send + Sync> (wrapper-chain upcast + downcast verification)
    let a: Arc<dyn Source> = Arc::new(S(41));
    let any: Arc<dyn Any + Send + Sync> = a;
    println!("arc r={}", any.downcast_ref::<S>().unwrap().v() + 1);

    // &dyn Source → &dyn Any (direct reference upcast)
    let s = S(7);
    let r: &dyn Source = &s;
    let a2: &dyn Any = r;
    println!("ref r={}", a2.downcast_ref::<S>().unwrap().v());
}
