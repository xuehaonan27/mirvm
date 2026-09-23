//! What a `fork` does to the capture state: the rebuild recipe published as an address the child
//! inherits, and the process generation the child claims for itself.

use super::*;

/// L2: the rebuild recipe is plain immutable memory whose address is
/// inherited unchanged by `fork`, so a child can read it without taking any
/// lock or running allocator code.
#[test]
fn rebuild_recipe_is_published_and_readable() {
    let path = std::env::temp_dir().join("events-1-0.mlog");
    let directory = path.parent().unwrap().to_path_buf();
    publish_rebuild_recipe(&path, 4096);
    let recipe = pending_rebuild_recipe().expect("recipe must be published");
    assert_eq!(recipe.directory, directory);
    assert_eq!(recipe.page_budget_bytes, 4096);
    clear_rebuild_recipe();
    assert!(pending_rebuild_recipe().is_none(), "clear must unpublish");
}

/// L2: the published recipe's memory is inherited by `fork`, which is the
/// whole point of publishing an address instead of storing the recipe in a
/// session a child may not touch.
///
/// The parent captures the address before forking and hands it to the child,
/// so this checks the inherited memory rather than the global pointer, which
/// parallel tests may legitimately clear.
#[test]
fn rebuild_recipe_memory_survives_fork() {
    let path = std::env::temp_dir().join("events-1-0.mlog");
    publish_rebuild_recipe(&path, 8192);
    let address = REBUILD_RECIPE.load(Ordering::Acquire);
    assert_ne!(address, 0, "recipe must be published before the fork");

    // SAFETY: the recipe is leaked for the process lifetime, and the child
    // inherited the same address space.
    let payload = forked_report(|| {
        unsafe { (*(address as *const RebuildRecipe)).page_budget_bytes }.to_le_bytes()
    });
    assert_eq!(
        u64::from_le_bytes(payload),
        8192,
        "the child must read the parent's recipe through the inherited address"
    );
}

/// L2: a `fork` child must not reuse the parent's generation, and must keep
/// claiming that same generation for later sessions in the same process.
/// Exercised across a real fork; the parent's value must not move.
#[test]
fn forked_child_advances_the_process_generation_once() {
    // Make the parent own its generation; a session would do the same, but
    // the global "one session per process" rule forbids starting another
    // here while parallel tests run capture sessions.
    let parent_generation = claim_process_generation();

    // The child inherits the parent's generation plus the pending mark.
    let payload = forked_report(|| {
        let first = claim_process_generation();
        let second = claim_process_generation();
        let mut payload = [0_u8; 16];
        payload[..8].copy_from_slice(&first.to_le_bytes());
        payload[8..].copy_from_slice(&second.to_le_bytes());
        payload
    });

    let child_first = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let child_second = u64::from_le_bytes(payload[8..].try_into().unwrap());
    assert_eq!(
        child_first,
        parent_generation + 1,
        "the child's first session must take the next generation"
    );
    assert_eq!(
        child_second, child_first,
        "later sessions in the same child reuse their own generation"
    );
    assert_eq!(
        claim_process_generation(),
        parent_generation,
        "the child's generation must not leak back into the parent"
    );
}
