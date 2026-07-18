//! C5 dyn 上溯（trait upcasting）探针：principal 变换时目标 vtable =
//! *(源 vtable + supertrait_vtable_slot×8)。两形——Arc 包装链
//! （Arc → NonNull → Pat → *const ArcInner → lockstep → data）与 & 直达。
//! 见 decision-history §7.13。
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
    // Arc<dyn Source> → Arc<dyn Any + Send + Sync>（包装链上溯 + downcast 验证）
    let a: Arc<dyn Source> = Arc::new(S(41));
    let any: Arc<dyn Any + Send + Sync> = a;
    println!("arc r={}", any.downcast_ref::<S>().unwrap().v() + 1);

    // &dyn Source → &dyn Any（引用直达上溯）
    let s = S(7);
    let r: &dyn Source = &s;
    let a2: &dyn Any = r;
    println!("ref r={}", a2.downcast_ref::<S>().unwrap().v());
}
