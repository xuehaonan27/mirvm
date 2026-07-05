use std::env;
fn main() {
    let args: Vec<String> = env::args().collect();
    println!("argc={} args[1..]={:?}", args.len(), &args[1..]);
    println!("HOME set: {}", env::var("HOME").is_ok());
    println!("MIRVM_TEST_VAR={:?}", env::var("MIRVM_TEST_VAR").ok());
}
