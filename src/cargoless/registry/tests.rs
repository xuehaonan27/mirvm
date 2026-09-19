use super::*;
use std::io::Write;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mirvm-cargoless-registry-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn sparse_path_scheme_matches_cargo_rules() {
    assert_eq!(Registry::sparse_path("a").unwrap(), "1/a");
    assert_eq!(Registry::sparse_path("ab").unwrap(), "2/ab");
    assert_eq!(Registry::sparse_path("abc").unwrap(), "3/a/abc");
    assert_eq!(Registry::sparse_path("serde").unwrap(), "se/rd/serde");
    assert_eq!(
        Registry::sparse_path("SerDe_JSON").unwrap(),
        "se/rd/serde_json"
    );
}

#[test]
fn parses_index_json_lines_with_features2_and_deps() {
    let line = r#"{"name":"demo","vers":"1.2.3","deps":[{"name":"d1","req":"^1.0","features":["f"],"optional":true,"default_features":false,"target":"cfg(unix)","kind":"build","package":"real-d1"},{"name":"d2","req":"*","features":[],"optional":false,"default_features":true,"target":null,"kind":"dev"}],"cksum":"abc123","features":{"default":["std"],"std":[]},"features2":{"weak":["d1?/inner"]},"yanked":false,"links":"demo-sys","rust_version":"1.70"}"#;
    let vs = parse_index_lines(&format!("{line}\n")).unwrap();
    assert_eq!(vs.len(), 1);
    let v = &vs[0];
    assert_eq!(v.version.to_string(), "1.2.3");
    assert_eq!(v.cksum, "abc123");
    assert!(!v.yanked);
    assert_eq!(v.links.as_deref(), Some("demo-sys"));
    assert_eq!(v.rust_version, Some(Version::new(1, 70, 0)));
    assert_eq!(v.features["default"], vec!["std"]);
    assert_eq!(v.features["weak"], vec!["d1?/inner"]);
    assert_eq!(v.deps.len(), 2);
    assert!(v.deps[0].optional);
    assert!(!v.deps[0].default_features);
    assert_eq!(v.deps[0].kind.as_deref(), Some("build"));
    assert_eq!(v.deps[0].package.as_deref(), Some("real-d1"));
    assert_eq!(v.deps[0].target.as_deref(), Some("cfg(unix)"));
    assert_eq!(v.deps[1].kind.as_deref(), Some("dev"));
}

#[test]
fn sha256_hex_matches_known_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"hello world"),
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
    // Multi-block + non-full tail: format check (block/finish paths locked down by the two vectors above)
    let big: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let got = sha256_hex(&big);
    assert_eq!(got.len(), 64);
    assert!(got.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn cksum_verify_rejects_tampered_bytes() {
    let good = sha256_hex(b"payload");
    verify_cksum(b"payload", Some(&good), "x-1.0.0").unwrap();
    let err = verify_cksum(b"tampered", Some(&good), "x-1.0.0").unwrap_err();
    assert!(err.contains("verification failed"), "{err}");
}

#[test]
fn unpacks_crate_and_rejects_traversal() {
    let d = tmpdir("unpack");
    // Build a synthetic tar.gz: demo-1.0.0/{Cargo.toml, src/lib.rs}
    let mut tar_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut add = |path: &str, content: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, content).unwrap();
        };
        add("demo-1.0.0/Cargo.toml", b"[package]\nname=\"demo\"\n");
        add("demo-1.0.0/src/lib.rs", b"pub fn f() {}\n");
        b.finish().unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).unwrap();
    let crate_bytes = gz.finish().unwrap();

    let dest = d.join("demo-1.0.0");
    unpack_crate(&crate_bytes, &dest, "demo-1.0.0").unwrap();
    assert!(dest.join("Cargo.toml").is_file());
    assert!(dest.join("src/lib.rs").is_file());

    // Top directory name mismatch → rejected
    let err = unpack_crate(&crate_bytes, &d.join("other"), "other-9.9.9").unwrap_err();
    assert!(err.contains("top directory"), "{err}");

    // Traversal path → loudly rejected (tar crate rejects at path() read or our guard rejects,
    // either defense line prevents unpacking; evil file must not reach disk)
    // Hand-craft a malicious tar containing .. (Builder refuses to write, bypass it by writing bytes directly):
    let mut evil_tar = vec![0u8; 512];
    let evil_name = b"demo-1.0.0/../evil.txt";
    evil_tar[..evil_name.len()].copy_from_slice(evil_name);
    evil_tar[100..108].copy_from_slice(b"0000644\0");
    evil_tar[108..116].copy_from_slice(b"0000000\0");
    evil_tar[116..124].copy_from_slice(b"0000000\0");
    evil_tar[124..136].copy_from_slice(b"00000000004\0"); // size=4 (octal)
    evil_tar[136..148].copy_from_slice(b"00000000000\0");
    evil_tar[148..156].copy_from_slice(b"        "); // cksum placeholder spaces
    evil_tar[156] = b'0';
    evil_tar[257..263].copy_from_slice(b"ustar\0");
    evil_tar[263..265].copy_from_slice(b"00");
    let sum: u32 = evil_tar[..512].iter().map(|&b| b as u32).sum();
    evil_tar[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    evil_tar.extend_from_slice(b"evil");
    evil_tar.resize(1024, 0);
    evil_tar.extend_from_slice(&[0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&evil_tar).unwrap();
    let evil_bytes = gz.finish().unwrap();
    let dest2 = d.join("demo2-1.0.0");
    assert!(unpack_crate(&evil_bytes, &dest2, "demo2-1.0.0").is_err());
    assert!(!d.join("evil.txt").exists());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn offline_mode_refuses_http_loudly() {
    let d = tmpdir("offline");
    // No cache + offline: both index and source must fail loudly (test does not touch network)
    let reg = Registry::open_at(d.clone(), true).unwrap();
    let err = reg
        .index_entry(CRATES_IO_LOCK_SOURCE, "no-such-crate-mirvm-test")
        .unwrap_err();
    assert!(err.contains("MIRVM_OFFLINE"), "{err}");
    let err = reg
        .ensure_source(
            CRATES_IO_LOCK_SOURCE,
            "no-such",
            &Version::new(0, 0, 0),
            None,
        )
        .unwrap_err();
    assert!(err.contains("MIRVM_OFFLINE"), "{err}");
    std::fs::remove_dir_all(&d).unwrap();
}
