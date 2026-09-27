fn main() {
    let n = 13u64;
    println!(
        "frag_share: {}",
        fdep::table_sum(n).wrapping_add(fdep::wide_sum(n))
    );
}
