// The preparation/execution split: the guest's own build warns. That warning is preparation detail,
// so a plain run holds it back and `prepare` (or `run -v`) is where a user reads it. The dead
// function is the warning: do not fix it.
fn never_called_by_the_guest() {}

fn main() {
    println!("prepare-warning-ran");
}
