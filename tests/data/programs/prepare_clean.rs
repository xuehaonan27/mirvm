// The preparation/execution split: a guest whose own build has nothing to report, so a run must be
// silent on stderr and `prepare` must leave the next run a cache hit.
fn main() {
    println!("prepare-clean-ran");
}
