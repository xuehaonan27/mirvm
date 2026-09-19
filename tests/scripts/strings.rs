// String / Vec / sort / format
fn main() {
    let mut s = String::from("hello");
    s.push_str(", mirvm");
    s.push('!');
    let mut v: Vec<i64> = vec![5, 3, 9, 1, -2, 8];
    v.sort();
    let joined: Vec<String> = v.iter().map(|x| format!("<{x}>")).collect();
    println!("{s} sorted={v:?} joined={}", joined.join("+"));
    let big: String = (0..100).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    println!("len={} tail={}", big.len(), &big[90..]);
}
