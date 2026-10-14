//! Owned compiler requests across process replacement. The worker validates the build before
//! executing a request and reuses the ordinary compiler driver.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use super::{Error, GuestProcessState};

type Units = Vec<(Box<str>, PathBuf, Vec<u32>)>;

#[cfg(not(feature = "compiler"))]
static UNITS: std::sync::OnceLock<Units> = std::sync::OnceLock::new();

pub(crate) fn install_units(units: Units) {
    #[cfg(feature = "compiler")]
    crate::image::units::install(crate::image::units::UnitTable::new(units));
    #[cfg(not(feature = "compiler"))]
    let _ = UNITS.set(units);
}

#[derive(Serialize, Deserialize)]
pub(super) struct RunRequest {
    pub rustc_args: Vec<String>,
    pub program_argv: Vec<String>,
    pub dump_mir: bool,
    pub vm_call: Option<String>,
    pub vm_stats: bool,
    pub suppress_runner_warning_summary: bool,
    pub guest_process: Option<GuestProcessState>,
    #[serde(with = "optional_path")]
    pub pack_out: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Handoff {
    build_id: String,
    pid: u32,
    input_lease_fd: i32,
    scope: String,
    prepare_only: bool,
    #[serde(with = "optional_path")]
    capture_directory: Option<PathBuf>,
    units: Units,
    run: RunRequest,
}

fn failure(detail: impl Into<String>) -> Error {
    Error::Worker {
        detail: detail.into(),
    }
}

/// Prefix every worker entry, including dependency and base preparation, with the producer's build.
pub(super) fn accept_prefix<I: Iterator<Item = String>>(
    args: &mut std::iter::Peekable<I>,
) -> Result<Option<String>, Error> {
    if args.peek().is_none_or(|arg| arg != "__compiler-worker") {
        return Ok(None);
    }
    args.next();
    args.next()
        .map(Some)
        .ok_or_else(|| failure("missing compiler worker build identity"))
}

/// Refuse incompatible workers before starting compiler or guest work.
pub(super) fn validate_build(expected: &str) -> Result<(), Error> {
    if !cfg!(feature = "compiler") {
        return Err(failure(
            "the selected compiler worker was built without compiler support",
        ));
    }
    if expected != crate::options::build::BUILD_ID {
        return Err(failure(
            "compiler worker build differs from the requesting runtime",
        ));
    }
    Ok(())
}

#[cfg(not(feature = "compiler"))]
fn replace(args: Vec<String>) -> Error {
    let worker = match crate::options::compiler_worker() {
        Some(path) => path,
        None => match std::env::current_exe() {
            Ok(path) => path.with_file_name("mirvm-compiler"),
            Err(error) => return failure(format!("cannot locate the compiler worker: {error}")),
        },
    };
    if let Err(error) = super::diagnostics::prepare_exec() {
        return failure(format!("cannot transfer compiler diagnostics: {error}"));
    }
    let error = crate::os::command::exec(
        std::process::Command::new(&worker)
            .arg("__compiler-worker")
            .arg(crate::options::build::BUILD_ID)
            .args(args),
    );
    failure(format!(
        "cannot execute compiler worker {}: {error}",
        worker.display()
    ))
}

#[cfg(not(feature = "compiler"))]
pub(crate) fn exec_command(args: Vec<String>) -> ! {
    crate::diag::emit(&crate::error::Error::from(replace(args)));
    std::process::exit(crate::diag::exit::SOFTWARE.into());
}

#[cfg(not(feature = "compiler"))]
pub(super) fn run(run: RunRequest) -> ExitCode {
    use std::io::Write;
    let lease = match crate::store::input_lease(crate::options::home(), false) {
        Ok(lease) => lease,
        Err(error) => {
            return crate::error::Error::from(failure(format!(
                "cannot lease compiler inputs: {error}"
            )))
            .report();
        }
    };
    let input_lease_fd = match lease.inherit() {
        Ok(fd) => fd,
        Err(error) => {
            return crate::error::Error::from(failure(format!(
                "cannot transfer compiler input lease: {error}"
            )))
            .report();
        }
    };
    let handoff = Handoff {
        build_id: crate::options::build::BUILD_ID.to_owned(),
        pid: std::process::id(),
        input_lease_fd,
        scope: crate::diag::current()
            .map_or("run", |scope| scope.name())
            .to_owned(),
        prepare_only: super::prepare_only(),
        capture_directory: super::capture_directory().map(PathBuf::from),
        units: UNITS.get().cloned().unwrap_or_default(),
        run,
    };
    let result = (|| {
        let dir = crate::store::COMPILER_HANDOFF.dir();
        std::fs::create_dir_all(&dir).map_err(|error| {
            failure(format!("cannot create compiler handoff directory: {error}"))
        })?;
        let path = crate::store::staging_path(&dir.join("request"));
        let bytes = serde_json::to_vec(&handoff)
            .map_err(|error| failure(format!("cannot encode compiler request: {error}")))?;
        let mut file = crate::os::command::private_file(&path)
            .map_err(|error| failure(format!("cannot create compiler request: {error}")))?;
        let write = file.write_all(&bytes);
        drop(file);
        if let Err(error) = write {
            let _ = std::fs::remove_file(&path);
            return Err(failure(format!("cannot write compiler request: {error}")));
        }
        let error = replace(vec![
            "__compiler-run".to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
        let _ = std::fs::remove_file(&path);
        Err(error)
    })();
    let Err(error): Result<(), Error> = result else {
        unreachable!()
    };
    crate::error::Error::from(error).report()
}

pub(super) fn run_main(mut args: impl Iterator<Item = String>, expected: &str) -> ExitCode {
    let result = (|| {
        let path = args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| failure("missing compiler request"))?;
        if args.next().is_some()
            || path.parent() != Some(crate::store::COMPILER_HANDOFF.dir().as_path())
        {
            return Err(failure("compiler request is outside the handoff store"));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| failure(format!("cannot read compiler request: {error}")))?;
        let handoff: Handoff = serde_json::from_slice(&bytes)
            .map_err(|error| failure(format!("invalid compiler request: {error}")))?;
        if handoff.build_id != expected || handoff.pid != std::process::id() {
            return Err(failure(
                "compiler request belongs to another build or process",
            ));
        }
        std::fs::remove_file(&path)
            .map_err(|error| failure(format!("cannot consume compiler request: {error}")))?;
        crate::diag::enter(match handoff.scope.as_str() {
            "build" => crate::diag::Component::Build,
            "run" | "prepare" | "install" | "capture" | "test" | "pack" | "runner" => {
                super::component_of(&handoff.scope)
            }
            _ => return Err(failure("compiler request has an invalid diagnostic scope")),
        });
        if handoff.prepare_only {
            super::set_prepare_only();
        }
        if let Some(directory) = handoff.capture_directory {
            super::set_forwarded_capture_directory(directory)
                .map_err(|_| failure("compiler request has a duplicate capture directory"))?;
        }
        // A rejected worker still consumes its private request and finalizes captured diagnostics.
        let _diagnostics = super::diagnostics::DiagnosticRouter::start(
            super::capture_directory(),
            super::capture_directory_is_forwarded(),
        )
        .map_err(|error| failure(format!("cannot receive compiler diagnostics: {error}")))?;
        validate_build(expected)?;
        // The request is private and belongs to this PID; its descriptor survived exec.
        let _input_lease = unsafe {
            crate::store::InputLease::from_inherited(crate::options::home(), handoff.input_lease_fd)
        }
        .map_err(|error| failure(format!("invalid compiler input lease: {error}")))?;
        if handoff
            .units
            .iter()
            .enumerate()
            .any(|(index, (_, _, deps))| deps.iter().any(|&dep| dep as usize >= index))
        {
            return Err(failure(
                "compiler request contains an unordered dependency unit table",
            ));
        }
        if !handoff.units.is_empty() {
            install_units(handoff.units);
        }
        let run = handoff.run;
        Ok(if let Some(out) = run.pack_out {
            super::pack_driver(run.rustc_args, run.program_argv, out)
        } else {
            super::run_driver(
                run.rustc_args,
                run.program_argv,
                run.dump_mir,
                run.vm_call,
                run.vm_stats,
                run.suppress_runner_warning_summary,
                run.guest_process,
            )
        })
    })();
    result.unwrap_or_else(|error| crate::error::Error::from(error).report())
}

/// Native environment strings are bytes, including values that are not UTF-8. JSON's ordinary
/// OsString serialization would reject such a caller environment before the worker could start.
pub(super) mod environment {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        env: &[(OsString, OsString)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        env.iter()
            .map(|(key, value)| (key.as_encoded_bytes(), value.as_encoded_bytes()))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(OsString, OsString)>, D::Error> {
        let entries = Vec::<(Vec<u8>, Vec<u8>)>::deserialize(deserializer)?;
        if entries.iter().any(|(key, value)| {
            key.is_empty() || key.contains(&0) || key.contains(&b'=') || value.contains(&0)
        }) {
            return Err(serde::de::Error::custom("invalid native environment entry"));
        }
        Ok(entries
            .into_iter()
            .map(|(key, value)| {
                (
                    crate::os::command::native_string(key),
                    crate::os::command::native_string(value),
                )
            })
            .collect())
    }
}

pub(super) mod optional_path {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|path| path.as_os_str().as_encoded_bytes())
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        let bytes = Option::<Vec<u8>>::deserialize(deserializer)?;
        if bytes.as_ref().is_some_and(|bytes| bytes.contains(&0)) {
            return Err(serde::de::Error::custom("path contains NUL"));
        }
        Ok(bytes.map(|bytes| PathBuf::from(crate::os::command::native_string(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Process {
        #[serde(with = "environment")]
        env: Vec<(OsString, OsString)>,
        #[serde(with = "optional_path")]
        cwd: Option<PathBuf>,
    }

    #[test]
    fn native_process_state_round_trips_without_utf8_conversion() {
        let bytes = vec![b'a', 0xff, b' ', b'z'];
        let native = crate::os::command::native_string(bytes);
        let process = Process {
            env: vec![
                (native.clone(), native.clone()),
                ("EMPTY".into(), OsString::new()),
            ],
            cwd: Some(PathBuf::from(native)),
        };
        let encoded = serde_json::to_vec(&process).unwrap();
        assert_eq!(
            serde_json::from_slice::<Process>(&encoded).unwrap(),
            process
        );
    }

    #[test]
    fn invalid_native_environment_and_paths_are_rejected_before_installation() {
        for (key, value) in [
            (b"".as_slice(), b"v".as_slice()),
            (b"a=b", b"v"),
            (b"a\0", b"v"),
            (b"a", b"v\0"),
        ] {
            let encoded = serde_json::json!({"env": [[key, value]], "cwd": null});
            assert!(serde_json::from_value::<Process>(encoded).is_err());
        }
        let encoded = serde_json::json!({"env": [], "cwd": [b'a', 0]});
        assert!(serde_json::from_value::<Process>(encoded).is_err());
    }
}
