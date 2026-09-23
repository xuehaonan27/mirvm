use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

use super::format::{
    FMT_VER, MAGIC, SECTION_ENTRY_LEN, TAG_FUNCS, TAG_META, TAG_MODULE, TAG_NATIVELIBS, TAG_RELOC,
    TAG_STAMPS, WHOLE_HASH_LEN, build_container, hash128, parse_container,
};
use super::funcs::{build_function_section, parse_function_section};
use super::meta::{Meta, ModuleMetaRef, Reloc, postcard_bytes};
use super::read::materialize_native_blob_at;

use crate::diag::Diagnostic as _;

static NEXT_PACKAGE_TEST: AtomicU64 = AtomicU64::new(0);

fn test_body(name: &str) -> crate::vm::ir::FuncBody {
    crate::vm::ir::FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: crate::vm::ir::RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![crate::vm::ir::Block {
            stmts: Vec::new(),
            term: crate::vm::ir::Terminator::Return,
        }],
        name: name.into(),
    }
}

fn replace_whole_hash(raw: &mut Vec<u8>) {
    raw.truncate(raw.len() - WHOLE_HASH_LEN);
    raw.extend_from_slice(&hash128(raw).to_le_bytes());
}

fn header_with_count(count: u32) -> Vec<u8> {
    let bid = crate::options::build::BUILD_ID.as_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FMT_VER.to_le_bytes());
    body.extend_from_slice(&(bid.len() as u32).to_le_bytes());
    body.extend_from_slice(bid);
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&hash128(&body).to_le_bytes());
    body
}

fn table_start() -> usize {
    MAGIC.len() + 4 + 4 + crate::options::build::BUILD_ID.len() + 4
}

fn package_bytes_for_module(module: &crate::vm::ir::Module) -> Vec<u8> {
    build_container(&[
        (
            TAG_META,
            postcard_bytes(&Meta {
                args: Vec::new(),
                envs: Vec::new(),
                base_key: None,
                target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            })
            .unwrap(),
        ),
        (
            TAG_STAMPS,
            postcard_bytes(&Vec::<crate::utils::content::FileStamp>::new()).unwrap(),
        ),
        (
            TAG_MODULE,
            postcard_bytes(&ModuleMetaRef::from(module)).unwrap(),
        ),
        (
            TAG_NATIVELIBS,
            postcard_bytes(&Vec::<NativeLibEntry>::new()).unwrap(),
        ),
        (
            TAG_RELOC,
            postcard_bytes(&Reloc {
                requires_fixed_base: false,
                entry: "main".into(),
            })
            .unwrap(),
        ),
        (TAG_FUNCS, build_function_section(&module.funcs).unwrap()),
    ])
    .unwrap()
}

fn load_test_package(bytes: &[u8]) -> Result<Package, Error> {
    let path = std::env::temp_dir().join(format!(
        "mirvm-package-verify-{}-{}.mirvm",
        std::process::id(),
        NEXT_PACKAGE_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).unwrap();
    let result = Package::load(&path);
    std::fs::remove_file(path).unwrap();
    result
}

#[test]
fn parser_accepts_writer_output() {
    let raw = build_container(&[(TAG_META, vec![1, 2]), (99, vec![3, 4, 5])]).unwrap();
    let parsed = parse_container(&raw).unwrap();
    assert_eq!(parsed.section(TAG_META).unwrap(), [1, 2]);
    assert_eq!(parsed.section(99).unwrap(), [3, 4, 5]);
}

#[test]
fn module_section_preserves_guest_panic_cleanup_plan() {
    let mut module = crate::vm::ir::Module::default();
    let plan = crate::vm::ir::GuestPanicCleanup {
        cleanup: 12,
        drop_payload: 34,
    };
    module.guest_panic_cleanup = Some(plan);

    let encoded = postcard_bytes(&ModuleMetaRef::from(&module)).unwrap();
    let decoded: ModuleMeta = postcard::from_bytes(&encoded).unwrap();
    assert_eq!(
        decoded.instantiate().unwrap().guest_panic_cleanup,
        Some(plan)
    );
}

#[test]
fn function_section_indexes_independent_verified_blobs() {
    let funcs = crate::vm::ir::FuncTable::from(vec![test_body("first"), test_body("second")]);
    let section = build_function_section(&funcs).unwrap();
    let blobs = parse_function_section(&section, 0).unwrap();
    assert_eq!(blobs.len(), 2);
    for (index, blob) in blobs.iter().enumerate() {
        let body: crate::vm::ir::FuncBody =
            postcard::from_bytes(&section[blob.start..blob.end]).unwrap();
        assert_eq!(&*body.name, ["first", "second"][index]);
    }

    let mut corrupt = section;
    *corrupt.last_mut().unwrap() ^= 1;
    let error = parse_function_section(&corrupt, 0).unwrap_err();
    assert_eq!(error.code(), Some("pack.corrupt"));
    assert!(error.to_string().contains("wrong hash"), "{error}");
}

#[test]
fn parser_rejects_truncated_build_id_without_panicking() {
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FMT_VER.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&hash128(&body).to_le_bytes());
    let err = parse_container(&body).err().unwrap();
    assert_eq!(err.code(), Some("pack.corrupt"));
    assert!(err.to_string().contains("build_id"), "{err}");
}

#[test]
fn parser_bounds_section_count_before_allocating() {
    let raw = header_with_count(u32::MAX);
    let err = parse_container(&raw).err().unwrap();
    assert_eq!(err.code(), Some("pack.corrupt"));
    assert!(err.to_string().contains("section table"), "{err}");
}

#[test]
fn parser_rejects_overflowing_section_range() {
    let mut raw = build_container(&[(TAG_META, vec![1])]).unwrap();
    let offset_pos = table_start() + 4;
    raw[offset_pos..offset_pos + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    replace_whole_hash(&mut raw);
    let err = parse_container(&raw).err().unwrap();
    assert_eq!(err.code(), Some("pack.corrupt"));
    assert!(
        err.to_string().contains("boundary") || err.to_string().contains("overflow"),
        "{err}"
    );
}

#[test]
fn parser_rejects_duplicate_tags_and_overlapping_sections() {
    let original = build_container(&[(TAG_META, vec![1]), (TAG_MODULE, vec![2])]).unwrap();

    let mut duplicate = original.clone();
    let second_tag = table_start() + SECTION_ENTRY_LEN;
    duplicate[second_tag..second_tag + 4].copy_from_slice(&TAG_META.to_le_bytes());
    replace_whole_hash(&mut duplicate);
    let error = parse_container(&duplicate).err().unwrap();
    assert_eq!(error.code(), Some("pack.corrupt"));
    assert!(error.to_string().contains("duplicate"), "{error}");

    let mut overlap = original;
    let first_offset = table_start() + 4;
    let first = overlap[first_offset..first_offset + 8].to_vec();
    let second_offset = table_start() + SECTION_ENTRY_LEN + 4;
    overlap[second_offset..second_offset + 8].copy_from_slice(&first);
    replace_whole_hash(&mut overlap);
    let error = parse_container(&overlap).err().unwrap();
    assert_eq!(error.code(), Some("pack.corrupt"));
    assert!(error.to_string().contains("overlap"), "{error}");
}

#[test]
fn parser_checks_every_section_hash() {
    let mut raw = build_container(&[(99, vec![1, 2, 3])]).unwrap();
    let hash_pos = table_start() + 4 + 8 + 8;
    raw[hash_pos] ^= 1;
    replace_whole_hash(&mut raw);
    let err = parse_container(&raw).err().unwrap();
    assert_eq!(err.code(), Some("pack.corrupt"));
    assert!(err.to_string().contains("wrong hash"), "{err}");
}

#[test]
fn malformed_p1_tables_are_rejected_by_safe_load_without_panicking() {
    use crate::vm::ir::{EntryStubSite, FfiKind, ForeignSig, LinkAddr};

    let addr = LinkAddr(0x6c00_0000_1000);
    let sig = ForeignSig {
        args: Vec::new(),
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: true,
    };
    let mut missing = crate::vm::ir::Module::default();
    missing.funcs.push(test_body("callback"));
    missing.ensure_function_names();
    missing.link_fn_addrs.insert(addr, 0);
    let missing_bytes = package_bytes_for_module(&missing);
    let result = std::panic::catch_unwind(|| load_test_package(&missing_bytes));
    let error = result
        .expect("safe Package::load panicked")
        .err()
        .expect("malformed package was accepted");
    assert_eq!(error.code(), Some("pack.reject"));
    assert!(
        error.to_string().contains("no matching entry stub"),
        "{error}"
    );

    let mut duplicate = missing;
    let site = EntryStubSite {
        link_addr: addr,
        func: 0,
        sig,
    };
    duplicate.entry_stub_sites = vec![site.clone(), site];
    let duplicate_bytes = package_bytes_for_module(&duplicate);
    let result = std::panic::catch_unwind(|| load_test_package(&duplicate_bytes));
    let error = result
        .expect("safe Package::load panicked")
        .err()
        .expect("malformed package was accepted");
    assert_eq!(error.code(), Some("pack.reject"));
    assert!(
        error.to_string().contains("duplicates entry link address"),
        "{error}"
    );
}

#[test]
fn entry_without_frozen_memory_is_rejected_by_safe_load_without_panicking() {
    use crate::vm::ir::{EntryPlan, LinkAddr};

    let mut module = crate::vm::ir::Module::default();
    module.funcs.push(test_body("lang_start"));
    module.ensure_function_names();
    module.entry = Some(EntryPlan {
        lang_start: 0,
        main_addr: LinkAddr(0x6c00_0000_1000),
        argc: 0,
        argv_ptr: 0,
        sigpipe: 0,
    });
    let bytes = package_bytes_for_module(&module);
    let result = std::panic::catch_unwind(|| load_test_package(&bytes));
    let error = result
        .expect("safe Package::load panicked")
        .err()
        .expect("entry without frozen memory was accepted");
    assert_eq!(error.code(), Some("pack.reject"));
    assert!(
        error.to_string().contains("no frozen memory for argv"),
        "{error}"
    );
}

#[test]
fn native_blob_materialization_uses_embedded_bytes() {
    // The family directory, as the caller hands it over.
    let dir = std::env::temp_dir().join(format!(
        "mirvm-pack-test-{}-{}",
        std::process::id(),
        NEXT_PACKAGE_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = b"embedded native image".to_vec();
    let lib = NativeLibEntry {
        path: "/path/that/does/not/exist.so".into(),
        role: 0,
        fnv: hash128(&bytes),
        bytes: bytes.clone(),
    };
    let path = materialize_native_blob_at(&dir, &lib).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    std::fs::write(&path, b"corrupt").unwrap();
    assert_eq!(
        std::fs::read(materialize_native_blob_at(&dir, &lib).unwrap()).unwrap(),
        lib.bytes
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn one_loaded_package_instantiates_isolated_frozen_memory_twice() {
    use crate::vm::ir::{
        Block, FuncBody, IntBinOp, LinkAddr, Operand, PlaceBase, PlaceExpr, RetAbi, Rvalue,
        ScalarPlace, Slot, Stmt, Terminator, Width,
    };

    let mut module = crate::vm::ir::Module::default();
    let mut frozen = crate::vm::frozen::FrozenArena::new();
    let link_cell = frozen.alloc(8, 8);
    unsafe { (link_cell as *mut u64).write(41) };
    let link_pointer = frozen.alloc(8, 8);
    unsafe { (link_pointer as *mut u64).write(link_cell) };
    module.frozen = Some(frozen);
    module.frozen_relocs.push(crate::vm::ir::FrozenReloc {
        at: LinkAddr(link_pointer),
        target: crate::vm::ir::FrozenRelocTarget::Frozen(LinkAddr(link_cell)),
    });
    let ret = Slot {
        off: 0,
        width: Width::W64,
    };
    let cell_place = || PlaceExpr {
        base: PlaceBase::Static(LinkAddr(link_cell)),
        steps: Box::new([]),
    };
    module.funcs.push(FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![
                Stmt::Assign {
                    dst: ScalarPlace::Slot(ret),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Add,
                        signed: false,
                        a: Operand::Mem {
                            expr: cell_place(),
                            width: Width::W64,
                        },
                        b: Operand::Imm {
                            bits: 1,
                            width: Width::W64,
                        },
                    },
                },
                Stmt::Assign {
                    dst: ScalarPlace::Mem {
                        expr: cell_place(),
                        width: Width::W64,
                    },
                    rv: Rvalue::Use(Operand::Slot(ret)),
                },
            ],
            term: Terminator::Return,
        }],
        name: "bump_static".into(),
    });
    module.funcs.push(FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![Stmt::Assign {
                dst: ScalarPlace::Slot(ret),
                rv: Rvalue::Use(Operand::AddrImm(LinkAddr(link_cell))),
            }],
            term: Terminator::Return,
        }],
        name: "static_address".into(),
    });
    let tls_ptr = Slot {
        off: 8,
        width: Width::W64,
    };
    module.tls.push(crate::vm::ir::TlsSlot {
        template: LinkAddr(link_cell),
        size: 8,
        align: 8,
    });
    module.funcs.push(FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![
                Stmt::Assign {
                    dst: ScalarPlace::Slot(tls_ptr),
                    rv: Rvalue::TlsRef(0),
                },
                Stmt::Assign {
                    dst: ScalarPlace::Slot(ret),
                    rv: Rvalue::Use(Operand::Mem {
                        expr: PlaceExpr {
                            base: PlaceBase::Local(tls_ptr.off),
                            steps: vec![crate::vm::ir::PlaceStep::Deref].into_boxed_slice(),
                        },
                        width: Width::W64,
                    }),
                },
            ],
            term: Terminator::Return,
        }],
        name: "tls_value".into(),
    });
    module.exports.insert("bump".into(), 0);
    module.exports.insert("address".into(), 1);
    module.exports.insert("tls".into(), 2);
    module.ensure_function_names();

    let sections = vec![
        (
            TAG_META,
            postcard_bytes(&Meta {
                args: Vec::new(),
                envs: Vec::new(),
                base_key: None,
                target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            })
            .unwrap(),
        ),
        (
            TAG_STAMPS,
            postcard_bytes(&Vec::<crate::utils::content::FileStamp>::new()).unwrap(),
        ),
        (
            TAG_MODULE,
            postcard_bytes(&ModuleMetaRef::from(&module)).unwrap(),
        ),
        (
            TAG_NATIVELIBS,
            postcard_bytes(&Vec::<NativeLibEntry>::new()).unwrap(),
        ),
        (
            TAG_RELOC,
            postcard_bytes(&Reloc {
                requires_fixed_base: false,
                entry: "main".into(),
            })
            .unwrap(),
        ),
        (TAG_FUNCS, build_function_section(&module.funcs).unwrap()),
    ];
    let bytes = build_container(&sections).unwrap();
    drop(module);

    let path = std::env::temp_dir().join(format!(
        "mirvm-package-instance-{}-{}.mirvm",
        std::process::id(),
        NEXT_PACKAGE_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).unwrap();
    let package = load_package(&path).unwrap();
    // A safe loaded artifact owns its verified bytes. Mutating and deleting the source
    // inode after load must not affect later lazy decoding or instantiation.
    std::fs::write(&path, b"replaced after Package::load").unwrap();
    std::fs::remove_file(&path).unwrap();
    let first = unsafe { crate::vm::Engine::from_module_unchecked(package.instantiate().unwrap()) }
        .unwrap();
    let second =
        unsafe { crate::vm::Engine::from_module_unchecked(package.instantiate().unwrap()) }
            .unwrap();

    let first_cell = first.shared().module.resolve_link_addr(LinkAddr(link_cell));
    let second_cell = second
        .shared()
        .module
        .resolve_link_addr(LinkAddr(link_cell));
    let first_pointer = first
        .shared()
        .module
        .resolve_link_addr(LinkAddr(link_pointer));
    let second_pointer = second
        .shared()
        .module
        .resolve_link_addr(LinkAddr(link_pointer));
    assert_ne!(first_cell, second_cell);
    assert_eq!(
        unsafe { (first_pointer as *const u64).read_unaligned() },
        first_cell
    );
    assert_eq!(
        unsafe { (second_pointer as *const u64).read_unaligned() },
        second_cell
    );

    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&first, "address", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(first_cell)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&second, "address", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(second_cell)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&first, "tls", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(41)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&second, "tls", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(41)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&first, "bump", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(42)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&second, "bump", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(42)
    );
    assert_eq!(
        unsafe { crate::vm::raw::run_export_raw(&first, "bump", &[]) }
            .unwrap()
            .into_returned()
            .map(|value| value.lo),
        Some(43)
    );
}
