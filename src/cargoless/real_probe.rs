#[test]
fn real_gluesql_probe() {
    use crate::cargoless::manifest::PackageManifest;
    use crate::cargoless::registry::Registry;
    use crate::cargoless::resolve::resolve;
    let text = std::fs::read_to_string("corpus/c_gluesql_db.rs").unwrap();
    let (fm, _) = crate::cli::parse_frontmatter_pub(&text).unwrap();
    let m = PackageManifest::from_frontmatter(
        "c_gluesql_db",
        &fm,
        std::path::Path::new("corpus/c_gluesql_db.rs"),
    )
    .unwrap();
    let mut reg = Registry::open().unwrap();
    let plan = resolve(&m, &mut reg).unwrap();
    for u in &plan.units {
        if matches!(
            u.package.as_str(),
            "rkyv" | "hashbrown" | "ahash" | "indexmap" | "rkyv_derive" | "rust_decimal" | "serde"
        ) {
            eprintln!(
                "PROBE unit {}@{} {:?} features={:?} deps={:?}",
                u.package,
                u.version,
                u.class,
                u.features,
                u.deps.iter().map(|d| d.key.clone()).collect::<Vec<_>>()
            );
        }
    }
}
