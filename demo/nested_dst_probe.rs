// Permanent differential probe for custom DST tails (slice/dyn/nested): a nested slice-tail
// struct field at a nonzero offset must have the alignment struct_tail computes statically.
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
    // Custom slice-tail DST (Box<Packet<[u8]>> via unsize)
    let b: Box<Packet<[u8; 4]>> = Box::new(Packet { len: 4, data: [1, 2, 3, 4] });
    let d: Box<Packet<[u8]>> = b;
    println!("slice-tail: len={} data={:?} sum={}", d.len, &d.data, d.data.iter().sum::<u8>());

    // Custom dyn-tail DST
    let b2: Box<Packet<Dog>> = Box::new(Packet { len: 1, data: Dog(7) });
    let d2: Box<Packet<dyn Speak>> = b2;
    println!("dyn-tail: len={} speak={}", d2.len, d2.data.speak());

    // Nested: the outer struct's tail is another struct with a slice tail
    #[derive(Debug)]
    struct Outer<T: ?Sized> {
        tag: u8,
        inner: Packet<T>,
    }
    let o: Box<Outer<[u16; 3]>> = Box::new(Outer { tag: 9, inner: Packet { len: 3, data: [10u16, 20, 30] } });
    let od: Box<Outer<[u16]>> = o;
    println!("nested: tag={} len={} data={:?}", od.tag, od.inner.len, &od.inner.data);

    // Nested dyn tail (the outer field tail is dyn -> the runtime vtable alignment path)
    let o2: Box<Outer<Dog>> = Box::new(Outer { tag: 5, inner: Packet { len: 1, data: Dog(42) } });
    let od2: Box<Outer<dyn Speak>> = o2;
    println!("nested-dyn: tag={} len={} speak={}", od2.tag, od2.inner.len, od2.inner.data.speak());
}
