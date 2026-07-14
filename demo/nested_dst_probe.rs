// M5.2 D8k 永久差分探针：自定义 DST 尾（slice/dyn/嵌套）——嵌套 slice-尾结构体
// 字段在非零偏移的对齐（旧 TRAP，本片按 struct_tail 静态对齐修复）与 native 对拍。
use std::fmt::Debug;

#[derive(Debug)]
struct Packet<T: ?Sized> {
    len: u32,
    data: T,
}

trait Speak {
    fn speak(&self) -> String;
}
struct Dog(u64);
impl Speak for Dog {
    fn speak(&self) -> String {
        format!("woof{}", self.0)
    }
}

fn main() {
    // 自定义 slice 尾 DST（Box<Packet<[u8]>> 经 unsize）
    let b: Box<Packet<[u8; 4]>> = Box::new(Packet { len: 4, data: [1, 2, 3, 4] });
    let d: Box<Packet<[u8]>> = b;
    println!("slice-tail: len={} data={:?} sum={}", d.len, &d.data, d.data.iter().sum::<u8>());

    // 自定义 dyn 尾 DST
    let b2: Box<Packet<Dog>> = Box::new(Packet { len: 1, data: Dog(7) });
    let d2: Box<Packet<dyn Speak>> = b2;
    println!("dyn-tail: len={} speak={}", d2.len, d2.data.speak());

    // 嵌套：外层 struct 的尾是另一个带 slice 尾的 struct
    #[derive(Debug)]
    struct Outer<T: ?Sized> {
        tag: u8,
        inner: Packet<T>,
    }
    let o: Box<Outer<[u16; 3]>> = Box::new(Outer { tag: 9, inner: Packet { len: 3, data: [10u16, 20, 30] } });
    let od: Box<Outer<[u16]>> = o;
    println!("nested: tag={} len={} data={:?}", od.tag, od.inner.len, &od.inner.data);

    // 嵌套 dyn 尾（外层字段尾是 dyn → 运行期 vtable 对齐路径）
    let o2: Box<Outer<Dog>> = Box::new(Outer { tag: 5, inner: Packet { len: 1, data: Dog(42) } });
    let od2: Box<Outer<dyn Speak>> = o2;
    println!("nested-dyn: tag={} len={} speak={}", od2.tag, od2.inner.len, od2.inner.data.speak());
}
