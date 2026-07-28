fn main() {
    println!(
        "cless-br n={} gated={} seen={}",
        bdep::N,
        bdep::GATED,
        env!("ROOT_SEEN")
    );
    #[cfg(root_feat)]
    println!("cless-br root-feat-on");
    std::process::exit(7);
}
