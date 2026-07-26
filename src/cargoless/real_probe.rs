#[test]
fn real_libgit2_probe() {
    use crate::cargoless::manifest::PackageManifest;
    use crate::cargoless::registry::Registry;
    use crate::cargoless::resolve::resolve;
    let text = std::fs::read_to_string("corpus/c_libgit2.rs").unwrap();
    let (fm, _) = crate::cli::parse_frontmatter_pub(&text).unwrap();
    let m = PackageManifest::from_frontmatter(
        "c_libgit2",
        &fm,
        std::path::Path::new("corpus/c_libgit2.rs"),
    )
    .unwrap();
    let mut reg = Registry::open().unwrap();
    match resolve(&m, &mut reg) {
        Ok(plan) => {
            eprintln!(
                "PROBE libgit2-sys: {:?}",
                plan.version_map.get("libgit2-sys")
            );
            eprintln!("PROBE git2: {:?}", plan.version_map.get("git2"));
        }
        Err(e) => eprintln!("PROBE ERR: {e}"),
    }
}
