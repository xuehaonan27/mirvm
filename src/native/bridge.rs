//! The runtime-interposition bridge as an artifact an image is linked with.
//!
//! [`crate::os_arch::bridge`] renders the text; this turns it into the file a link names. What that
//! file *is* differs per platform and the difference is load-bearing rather than packaging: GNU ld
//! is told by a flag to rename the references, so the bridge can be an object of the image, while
//! this format binds a call to the first library that defines it and so needs the bridge to be a
//! library of its own. Both are `os::linker`'s answer, and both end up on the link line the same
//! way — as an input.
//!
//! Content-addressed and cached like the other produced objects: an image's link line names the
//! artifact, so a rebuilt bridge has to be a different file rather than the same one rewritten.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::os::linker;

/// The version of the artifact's content, so a change to the text or to how it is built names new
/// files instead of reusing an old one. Bump when either changes shape.
const ARTIFACT_VERSION: &[u8] = b"mirvm-bridge-v1";

/// Build (or reuse) the bridge artifact in `dir` for the compiler `cc`, and return its path.
///
/// `cc_identity` is the compiler's own identity as the archive converter computes it, so two
/// toolchains that produce different objects do not share one cached bridge.
pub(crate) fn artifact(dir: &Path, cc: &Path, cc_identity: &[u8]) -> Result<PathBuf, String> {
    let asm = crate::os_arch::bridge::bridge_asm()?;
    let hash = crate::utils::content::fnv1a(
        &[
            ARTIFACT_VERSION,
            cc_identity,
            linker::BRIDGE_ARTIFACT_EXTENSION.as_bytes(),
            asm.as_bytes(),
        ]
        .concat(),
    );
    let object = dir.join(format!(
        "{hash:016x}.bridge.{}",
        linker::BRIDGE_ARTIFACT_EXTENSION
    ));
    if object.exists() {
        return Ok(object);
    }
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create the bridge cache directory: {error}"))?;
    let temporary = crate::store::staging_path(&object);
    let mut source = temporary.clone().into_os_string();
    source.push(".s");
    let source = PathBuf::from(source);
    std::fs::write(&source, &asm).map_err(|error| {
        format!(
            "cannot write the bridge assembly `{}`: {error}",
            source.display()
        )
    })?;
    let mut command = Command::new(cc);
    command.args(linker::BRIDGE_ARTIFACT);
    if let Some(flag) = linker::BRIDGE_INSTALL_NAME {
        command.arg(flag).arg(&object);
    }
    let output = command
        .arg(&source)
        .arg("-o")
        .arg(&temporary)
        .output()
        .map_err(|error| format!("cannot launch cc to build the bridge: {error}"))?;
    let _ = std::fs::remove_file(&source);
    if !output.status.success() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "cc could not build the runtime bridge:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    crate::store::publish(&object, &temporary)
        .map_err(|error| format!("cannot publish the runtime bridge: {error}"))?;
    Ok(object)
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::artifact;

    const OWNER: u64 = 0x0ddc_0ffe_e15e_c7ed;

    static SEEN_OWNER: AtomicU64 = AtomicU64::new(0);
    static SEEN_ARG: AtomicU64 = AtomicU64::new(0);

    /// The replacement the bridge routes `pthread_create` to. `owner` is its trailing parameter,
    /// which is the shape the bridge entry exists to fill.
    unsafe extern "C" fn replacement(
        _thread: usize,
        _attr: usize,
        _start: usize,
        arg: usize,
        owner: u64,
    ) -> i32 {
        SEEN_OWNER.store(owner, Ordering::SeqCst);
        SEEN_ARG.store(arg as u64, Ordering::SeqCst);
        0
    }

    fn cc() -> std::path::PathBuf {
        std::path::PathBuf::from("cc")
    }

    /// A temporary directory that cleans up after itself.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mirvm-bridge-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Dir(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The artifact has to be buildable before any archive can be converted, so this fails at the
    /// cheapest point if the bridge text stops assembling for this pair.
    #[test]
    fn the_bridge_artifact_builds_and_is_reused() {
        let dir = Dir::new("cache");
        let first = artifact(dir.path(), &cc(), b"probe").expect("a bridge for this pair");
        assert!(first.is_file(), "`{}` is not a file", first.display());
        let again = artifact(dir.path(), &cc(), b"probe").expect("the cached bridge");
        assert_eq!(first, again, "the artifact is content-addressed");
    }

    /// The point of the bridge is that a call written as `pthread_create` reaches mirvm's
    /// replacement, so the test links an image that makes that call and then makes it. A bridge
    /// that assembles but receives nothing is the failure this catches, and it is the one a
    /// text-only check cannot: only the link and the loader know where the call went.
    #[test]
    fn a_linked_call_reaches_the_bridge_and_carries_the_engine() {
        let dir = Dir::new("linked");
        let bridge = artifact(dir.path(), &cc(), b"probe").expect("a bridge");
        let source = dir.path().join("caller.c");
        std::fs::write(
            &source,
            "#include <pthread.h>\n\
             static void *worker(void *arg) { return arg; }\n\
             int caller_makes_a_thread(void *arg) {\n\
             \x20   pthread_t thread;\n\
             \x20   return pthread_create(&thread, 0, worker, arg);\n\
             }\n",
        )
        .expect("the caller source");
        let image = dir.path().join(format!(
            "libcaller.{}",
            crate::os::linker::BRIDGE_ARTIFACT_EXTENSION
        ));
        let calls: Vec<&str> = crate::vm::interpose::INTERPOSED_CALLS.to_vec();
        let interpose =
            crate::os::linker::interpose_args(&calls).expect("this platform's redirection");
        let output = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wl,-undefined,dynamic_lookup", "-o"])
            .arg(&image)
            .arg(&source)
            .arg(&bridge)
            .args(&interpose)
            .output()
            .expect("launching cc");
        assert!(
            output.status.success(),
            "cc could not link against the bridge:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let image_handle = open(&image);
        // Where a bridge slot ends up depends on what the artifact *is*: on a platform whose bridge
        // is an object of the image the slots travel with the image, and on one whose bridge is a
        // library the image links against they are the bridge's. Reaching them takes the two routes
        // the engine also takes, because the platform decides whether they are exported: the loader,
        // which searches an image's whole load closure, and the image's own symbol table, which is
        // the answer where the entries reach their slots PC-relative and so cannot export them.
        //
        // The bridge is only opened where it is an image at all; where it is an object, a loader
        // refuses it outright (`only ET_DYN and ET_EXEC can be loaded`).
        let mut images = vec![(image.clone(), image_handle)];
        if crate::os::linker::BRIDGE_SLOT_IS_EXPORTED {
            images.push((bridge.clone(), open(&bridge)));
        }
        let mut tables = Vec::new();
        for (path, handle) in &images {
            let cpath =
                std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path");
            let bias = crate::os::dll::load_bias(*handle, &cpath).expect("a load base") as u64;
            let symbols = crate::native::symtab::hidden_symtab_values(
                &path.to_string_lossy(),
                crate::os::dll::OBJECT_FORMAT,
            )
            .expect("the artifact's own symbol table");
            tables.push((*handle, bias, symbols));
        }
        let address = |name: &str| -> usize {
            let cname = std::ffi::CString::new(name).expect("a slot name");
            for (handle, bias, symbols) in &tables {
                let exported = crate::os::dll::sym(*handle, &cname);
                if exported != 0 {
                    return exported;
                }
                if let Some(value) = symbols.get(name).and_then(|value| bias.checked_add(*value)) {
                    return value as usize;
                }
            }
            0
        };
        let patch = |name: &str, value: u64| {
            let slot = address(name);
            assert!(slot != 0, "no image in the closure defines `{name}`");
            unsafe { (slot as *mut u64).write(value) };
        };
        patch("__mirvm_pthread_owner", OWNER);
        patch(
            &crate::vm::interpose::target_slot("pthread_create"),
            replacement as *const () as usize as u64,
        );

        let cimage = std::ffi::CString::new(image.as_os_str().as_encoded_bytes()).expect("a path");
        let call: unsafe extern "C" fn(usize) -> i32 = unsafe {
            std::mem::transmute(crate::os::dll::sym(image_handle, c"caller_makes_a_thread"))
        };
        let arg = 0xfeed_face_usize;
        assert_eq!(
            unsafe { call(arg) },
            0,
            "the replacement's own return value"
        );
        assert_eq!(
            SEEN_OWNER.load(Ordering::SeqCst),
            OWNER,
            "the replacement did not receive the engine, so the call did not come through the bridge"
        );
        assert_eq!(SEEN_ARG.load(Ordering::SeqCst), arg as u64);

        for (_, handle) in images {
            unsafe { crate::os::dll::close(handle) };
        }
        let _ = cimage;
    }

    /// `dlopen` with the flags an image of this shape needs, which is the loader's business rather
    /// than the test's.
    fn open(path: &std::path::Path) -> usize {
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path");
        crate::os::dll::open_with_flags(
            &cpath,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .unwrap_or_else(|error| panic!("dlopen `{}` failed: {error}", path.display()))
    }
}
