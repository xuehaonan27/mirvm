//! Single source of truth for every external input mirvm defines.
//!
//! Every environment variable and command-line option mirvm defines, and every test-only `MIRVM_*`
//! name, is declared once in the `entries!` table below. [`get`] resolves them into
//! [`Options`] once per process, `USAGE` is generated from the register, `mirvm options` prints it,
//! and `quality.rust` fails when `src/` spells an `MIRVM_*` name anywhere else.
//!
//! Names that are not ours are deliberately absent: the `CARGO_*` contract mirvm consumes and
//! injects belongs to cargo, and the `mirvm test` selection grammar belongs to cargo's command line.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// Who may supply the value of an input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Identity {
    /// A knob a user may set. Advertised in `USAGE`.
    User,
    /// A diagnostic knob: environment only, not advertised in `USAGE`'s option list.
    Dev,
    /// A test-only input. Meaningless outside a test run.
    Test,
    /// An internal parent-to-child variable. Never set by a user.
    Protocol,
}

impl Identity {
    pub fn name(self) -> &'static str {
        match self {
            Identity::User => "user",
            Identity::Dev => "dev",
            Identity::Test => "test",
            Identity::Protocol => "protocol",
        }
    }
}

/// The subcommand an option's command-line spelling belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Run,
    Test,
    Pack,
    Capture,
    Cache,
    Log,
    Deps,
    Options,
    /// A flag mirvm passes to itself; documented but never advertised.
    Internal,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Run => "run",
            Scope::Test => "test",
            Scope::Pack => "pack",
            Scope::Capture => "capture",
            Scope::Cache => "cache",
            Scope::Log => "log",
            Scope::Deps => "deps",
            Scope::Options => "options",
            Scope::Internal => "internal",
        }
    }

    /// Parse a scope name as spelled in the register. An unknown name is a programming error and
    /// fails loudly at the first [`entries`] call.
    fn parse(name: &str) -> Self {
        match name {
            "Run" => Scope::Run,
            "Test" => Scope::Test,
            "Pack" => Scope::Pack,
            "Capture" => Scope::Capture,
            "Cache" => Scope::Cache,
            "Log" => Scope::Log,
            "Deps" => Scope::Deps,
            "Options" => Scope::Options,
            "Internal" => Scope::Internal,
            other => panic!("options: unknown scope `{other}`"),
        }
    }
}

/// One external input.
#[derive(Clone)]
pub struct Entry {
    /// Accessor key, and the name `mirvm options` prints.
    pub field: &'static str,
    pub identity: Identity,
    /// `None` for an option that only exists on a command line.
    pub env: Option<&'static str>,
    pub cli: Option<&'static str>,
    /// Additional accepted spellings, e.g. `-o`.
    pub aliases: Vec<&'static str>,
    pub scopes: Vec<Scope>,
    /// Human-readable default, used by `USAGE` and `mirvm options`. The authoritative default lives
    /// in the accessor, a few lines below.
    pub default: &'static str,
    pub doc: &'static str,
}

macro_rules! identity_of {
    (user) => {
        Identity::User
    };
    (dev) => {
        Identity::Dev
    };
    (test) => {
        Identity::Test
    };
    (protocol) => {
        Identity::Protocol
    };
}

/// Applies one register attribute. An unknown attribute name is a compile error, so the vocabulary
/// cannot silently grow a spelling the rest of the module does not understand.
macro_rules! attr_of {
    ($entry:ident, env, $arg:expr) => {
        $entry.env = Some($arg);
    };
    ($entry:ident, cli, $flag:expr, $scopes:expr) => {{
        attr_of!($entry, cli, $flag, "", $scopes);
    }};
    ($entry:ident, cli, $flag:expr, $aliases:expr, $scopes:expr) => {{
        $entry.cli = Some($flag);
        for alias in $aliases.split_ascii_whitespace() {
            $entry.aliases.push(alias);
        }
        for scope in $scopes.split_ascii_whitespace() {
            $entry.scopes.push(Scope::parse(scope));
        }
    }};
    ($entry:ident, default, $arg:expr) => {
        $entry.default = $arg;
    };
}

/// Declares the register, one entry per line:
///
/// `identity field = [env("NAME")] [cli("--flag")] [alias("-x")] [scope("Run")] default("..") doc("..");`
///
/// The identity is `user`, `dev`, `test` or `protocol`. A protocol entry names its variable through
/// [`protocol`], so a parent-to-child variable is spelled once in the tree. Every entry carries a doc
/// comment and a `default`.
macro_rules! entries {
    ( $( #[doc = $doc:literal] $kind:ident $field:ident $( $attr:ident ( $($arg:expr),* ) )* ; )* ) => {
        /// The register: every external input mirvm defines, in declaration order.
        pub fn entries() -> &'static [Entry] {
            static REGISTER: std::sync::OnceLock<Vec<Entry>> = std::sync::OnceLock::new();
            REGISTER.get_or_init(|| {
                let mut out: Vec<Entry> = Vec::new();
                $(
                    out.push(Entry {
                        field: stringify!($field),
                        identity: identity_of!($kind),
                        env: None,
                        cli: None,
                        aliases: Vec::new(),
                        scopes: Vec::new(),
                        default: "",
                        doc: $doc.trim_start(),
                    });
                    let entry = out.last_mut().expect("the entry was just pushed");
                    $( attr_of!(entry, $attr $(, $arg)*); )*
                )*
                out
            })
        }
    };
}

entries! {
    /// Local store root: cache/ (deletable), data/ (expensive to lose), build/ (project space), run/ (process scratch).
    user     home                     env("MIRVM_HOME") default("$HOME/.mirvm");
    /// Relocate the Cargo track's target directory (the shared dependency store) out of `build/`.
    user     target_dir               env("MIRVM_TARGET_DIR") default("$MIRVM_HOME/build/target/mirvm");
    /// MIR-rich sysroot; equivalent to --sysroot. Default: build and cache one.
    user     sysroot                  env("MIRVM_SYSROOT") cli("--sysroot", "Run") default("auto-built");
    /// Guest main execution stack reservation; accepts a k/m/g suffix, range 1m..=1t.
    user     stack_size               env("MIRVM_STACK_SIZE") cli("--stack-size", "Run") default("1g");
    /// Method-level JIT; `off` runs the pure interpreter (differential benchmark).
    user     jit                      env("MIRVM_JIT") cli("--jit", "Run") default("on");
    /// JIT compilation trigger threshold (diagnostic).
    user     jit_threshold            env("MIRVM_JIT_THRESHOLD") default("1000");
    /// Compile on the enqueueing thread and fail loudly; makes a forced threshold observable.
    user     jit_sync                 env("MIRVM_JIT_SYNC") default("off");
    /// Print JIT helper frequency statistics at process exit.
    user     jit_stats                env("MIRVM_JIT_STATS") default("off");
    /// Build frontmatter/script projects with --locked.
    user     cargo_locked             env("MIRVM_CARGO_LOCKED") default("off");
    /// `self` = zero-cargo own scheduling; `cargo` = the Cargo compatibility track.
    user     deps                     env("MIRVM_DEPS") default("self");
    /// Cargoless compilation concurrency; =1 is the serial differential anchor.
    user     cless_jobs               env("MIRVM_CLESS_JOBS") default("available parallelism");
    /// rustc frontend threads for the compile session: off | sync | 0..=256.
    user     threads                  env("MIRVM_THREADS") default("off");
    /// Write the phase ledger (frontend/lower/engine/total) to stderr.
    user     timing                   env("MIRVM_TIMING") default("off");
    /// Bypass the L2 engine-IR cache (read and write).
    user     no_ir_cache              env("MIRVM_NO_IR_CACHE") default("off");
    /// Bypass the pre-lowered std base image (full cold lowering).
    user     no_base_image            env("MIRVM_NO_BASE_IMAGE") default("off");
    /// Bypass the dependency image.
    user     no_deps_image            env("MIRVM_NO_DEPS_IMAGE") default("off");
    /// Disable registry HTTP; resolve from the local cache only and fail loudly on a miss.
    user     offline                  env("MIRVM_OFFLINE") default("off");
    /// `mirvm pack`: carry machine code out of line and rematerialize it on load.
    user     pack_no_mc               env("MIRVM_PACK_NO_MC") default("off");

    /// Log the JIT compiler thread's receive/publish flow.
    dev      jit_debug                env("MIRVM_JIT_DEBUG") default("off");
    /// Dump CLIF for functions whose compilation fails.
    dev      jit_debug_dump           env("MIRVM_JIT_DEBUG_DUMP") default("off");
    /// Log build-script scheduling.
    dev      debug_bldrs              env("MIRVM_DEBUG_BLDRS") default("off");
    /// Log resolver feature unification.
    dev      debug_unify              env("MIRVM_DEBUG_UNIFY") default("off");
    /// `mirvm deps audit`: keep the scratch tree instead of cleaning it up.
    dev      deps_audit_keep          env("MIRVM_DEPS_AUDIT_KEEP") default("off");
    /// Log native-archive symbol resolution.
    dev      c2_debug                 env("MIRVM_C2_DEBUG") default("off");
    /// Log dependency-image pre-key computation.
    dev      a2_debug                 env("MIRVM_A2_DEBUG") default("off");
    /// Print lowering purity statistics.
    dev      purity_stats             env("MIRVM_PURITY_STATS") default("off");
    /// Trace guest syscalls.
    dev      syscall_trace            env("MIRVM_SYSCALL_TRACE") default("off");
    /// Print the fault RIP on SIGSEGV to locate a JIT code crash site.
    dev      segv_dump                env("MIRVM_SEGV_DUMP") default("off");

    /// Encoded rustflags appended inside the compiler wrapper; set by the differential suite.
    test     encoded_rustflags_append env("MIRVM_ENCODED_RUSTFLAGS_APPEND") default("empty");

    /// Route argv into the Cargo wrapper phase.
    protocol cargo_session            env(protocol::CARGO_SESSION) default("unset");
    /// This session occupies Cargo's RUSTC slot rather than the wrapper slot.
    protocol cargo_compiler           env(protocol::CARGO_COMPILER) default("unset");
    /// Emit a .mirvm package to this path instead of executing.
    protocol pack                     env(protocol::PACK) default("unset");
    /// Caller directory to enter before guest execution.
    protocol guest_cwd                env(protocol::GUEST_CWD) default("unset");
    /// The caller's own sysroot, echoed back into the guest environment.
    protocol caller_sysroot           env(protocol::CALLER_SYSROOT) default("unset");
    /// Whether the caller had MIRVM_SYSROOT set (1/0).
    protocol caller_sysroot_present   env(protocol::CALLER_SYSROOT_PRESENT) default("unset");
    /// Doctest builder launcher path.
    protocol doctest_builder          env(protocol::DOCTEST_BUILDER) default("unset");
    /// Doctest working directory.
    protocol doctest_run_dir          env(protocol::DOCTEST_RUN_DIR) default("unset");

    /// Print the entry function's MIR and exit.
    user     dump_mir                 cli("--dump-mir", "Run") default("off");
    /// Edition for the single-file form.
    user     edition                  cli("--edition", "Run") default("2024");
    /// Compatibility spelling; `vm` is the only engine and is accepted silently.
    user     run_engine               cli("--engine", "Run") default("vm");
    /// Call an exported function directly, e.g. 'fib(25)', instead of the main startup chain.
    user     vm_call                  cli("--vm-call", "Run") default("unset");
    /// Print Trap-debt statistics and exit.
    user     vm_stats                 cli("--vm-stats", "Run") default("off");
    /// Cargo `--bin` semantics; project form only.
    user     bin                      cli("--bin", "Run") default("unset");
    /// Ignore package.rust-version, with Cargo's semantics.
    user     ignore_rust_version      cli("--ignore-rust-version", "Run") default("off");
    /// Output path: the .mirvm package (`pack`) or the capture directory (`capture`).
    user     output                   cli("--output", "-o", "Pack Capture") default("unset");
    /// Report what `cache purge` would remove without removing it.
    user     cache_dry_run            cli("--dry-run", "Cache") default("off");
    /// Purge the whole dependency-image family.
    user     cache_deps               cli("--deps", "Cache") default("off");
    /// Purge the whole base-image family.
    user     cache_base               cli("--base", "Cache") default("off");
    /// Purge the whole L2 engine-IR family.
    user     cache_ir                 cli("--ir", "Cache") default("off");
    /// Purge scripts/ (materialized frontmatter projects).
    user     cache_scripts            cli("--scripts", "Cache") default("off");
    /// Purge the unified target directory (the shared dependency store).
    user     cache_target             cli("--target", "Cache") default("off");
    /// Purge all of cache/, build/ and run/: everything that costs no network to rebuild.
    user     cache_all                cli("--all", "Cache") default("off");
    /// Also purge data/ (crate store and sysroot); with --all this is a full cold start.
    user     cache_data               cli("--data", "Cache") default("off");
    /// `log export` filter: engine id, or `unknown`.
    user     log_engine               cli("--engine", "Log") default("unset");
    /// `log export` filter: producer id.
    user     log_producer             cli("--producer", "Log") default("unset");
    /// `log export` filter: thread id.
    user     log_tid                  cli("--tid", "Log") default("unset");
    /// `log export` filter: event kind.
    user     log_kind                 cli("--kind", "Log") default("unset");
    /// `log export` filter: a sequence number or a START:END range.
    user     log_sequence             cli("--sequence", "Log") default("unset");
    /// Machine output: reports as one JSON document, diagnostics as one JSON object per line.
    user     output_format            env("MIRVM_OUTPUT") cli("--json", "Run Pack Capture Cache Deps Options Log") default("text");
    /// Internal: forwarded capture directory for the Cargo runner form.
    user     mirvm_capture_directory  cli("--mirvm-capture-directory", "Internal") default("unset");
}

/// Look up a register row. Panics on an unknown field: an accessor naming a field that is not
/// registered is a programming error, and a loud failure is the only kind that stays fixed.
pub fn lookup(field: &str) -> &'static Entry {
    match entries().iter().find(|entry| entry.field == field) {
        Some(entry) => entry,
        None => panic!("options: `{field}` is not registered in `entries`"),
    }
}

/// Look up a register row without panicking.
pub fn find(field: &str) -> Option<&'static Entry> {
    entries().iter().find(|entry| entry.field == field)
}

fn env_name(field: &str) -> &'static str {
    match lookup(field).env {
        Some(name) => name,
        None => panic!("options: `{field}` has no environment variable"),
    }
}

/// The environment name of an option. Needed where a value must be moved between environments
/// rather than read, such as restoring the caller's environment for the guest.
pub fn env_var_name(field: &str) -> &'static str {
    env_name(field)
}

/// An offline diagnostic. The knob's own name comes from the register, so the message cannot drift
/// from the variable it tells the user to set.
pub fn offline_error(detail: impl std::fmt::Display) -> String {
    format!("{}: {detail}", env_var_name("offline"))
}

// ===== source tracking =====

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Default,
    Env,
    Cli,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Env => "env",
            Source::Cli => "cli",
        }
    }
}

static CLI_SET: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

/// Record that the command line set this option, so `mirvm options` can report `cli` rather than
/// `env` for a value the parser resolved.
pub fn note_cli(field: &'static str) {
    if let Ok(mut set) = CLI_SET.lock() {
        set.insert(field);
    }
}

fn from_cli(field: &str) -> bool {
    CLI_SET.lock().is_ok_and(|set| set.contains(field))
}

/// Where an environment-backed option's current value comes from.
pub fn source(field: &str) -> Source {
    if from_cli(field) {
        return Source::Cli;
    }
    match lookup(field).env {
        Some(name) if std::env::var_os(name).is_some() => Source::Env,
        _ => Source::Default,
    }
}

/// Raw value of an environment-backed option.
pub fn raw(field: &str) -> Option<String> {
    std::env::var(env_name(field)).ok()
}

/// Presence flag: set to any value, including the empty string.
pub fn flag(field: &str) -> bool {
    std::env::var_os(env_name(field)).is_some()
}

/// Non-empty flag: set and not the empty string.
pub fn nonempty(field: &str) -> bool {
    raw(field).is_some_and(|value| !value.is_empty())
}

/// Export a resolved value through its own environment variable so a child process reading the same
/// register observes the parent's decision.
///
/// The caller must be in the single-threaded startup phase, before any worker, compiler or guest
/// thread exists — the same discipline `std::env::set_var` already required at these call sites.
pub fn export_to_process(field: &str, value: &str) {
    unsafe { std::env::set_var(env_name(field), value) };
}

/// [`export_to_process`] for a value that need not be UTF-8, such as a path.
pub fn export_os_to_process(field: &str, value: &std::ffi::OsStr) {
    unsafe { std::env::set_var(env_name(field), value) };
}

// ===== the resolved options of this process =====

/// The dependency track `MIRVM_DEPS` selects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DepsTrack {
    /// Zero-cargo own scheduling.
    Own,
    /// The Cargo compatibility track.
    Cargo,
}

/// The output mode `MIRVM_OUTPUT` / `--json` selects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    /// Human text: `mirvm[component]: severity: message`, and aligned report tables.
    Text,
    /// Machine output: JSONL diagnostics on fd 2 and one versioned JSON document per report.
    Json,
}

/// Every external input mirvm defines, resolved from the environment.
///
/// Everything here is fixed for the lifetime of the process except `jit`, `stack_size`, `offline`
/// and `output_format`: the command line overrides those by writing the very variable its child
/// processes inherit, so their accessors read it live instead of snapshotting it.
pub struct Options {
    /// `MIRVM_HOME`: local store root.
    pub home: PathBuf,
    /// `MIRVM_TARGET_DIR`: the unified Cargo target directory.
    pub target_dir: PathBuf,
    /// `MIRVM_SYSROOT`: the MIR-rich sysroot, when the caller named one.
    pub sysroot: Option<PathBuf>,
    /// `MIRVM_JIT_THRESHOLD`: compilation trigger, 1000 when unset or unusable.
    pub jit_threshold: u32,
    /// `MIRVM_JIT_SYNC`: compile on the enqueueing thread so failures terminate loudly.
    pub jit_sync: bool,
    /// `MIRVM_JIT_STATS`: print helper frequency statistics at exit.
    pub jit_stats: bool,
    /// `MIRVM_CARGO_LOCKED`: build frontmatter/script projects with `--locked`.
    pub cargo_locked: bool,
    /// `MIRVM_TIMING`: write the phase ledger to stderr.
    pub timing: bool,
    /// `MIRVM_NO_IR_CACHE`: bypass the L2 engine-IR cache.
    pub no_ir_cache: bool,
    /// `MIRVM_NO_BASE_IMAGE`: bypass the pre-lowered std base image.
    pub no_base_image: bool,
    /// `MIRVM_NO_DEPS_IMAGE`: bypass the dependency image.
    pub no_deps_image: bool,
    /// `MIRVM_PACK_NO_MC`: carry machine code out of line in a package.
    pub pack_no_mc: bool,
    /// `MIRVM_DEPS_AUDIT_KEEP`: keep the `mirvm deps audit` scratch tree.
    pub deps_audit_keep: bool,
    /// `MIRVM_ENCODED_RUSTFLAGS_APPEND`: extra rustflags for the compiler wrapper.
    pub encoded_rustflags_append: Option<String>,
    /// `MIRVM_JIT_DEBUG`: log the JIT compiler thread's receive/publish flow.
    pub jit_debug: bool,
    /// `MIRVM_JIT_DEBUG_DUMP`: dump CLIF for functions whose compilation fails.
    pub jit_debug_dump: bool,
    /// `MIRVM_DEBUG_BLDRS`: log build-script scheduling.
    pub debug_bldrs: bool,
    /// `MIRVM_DEBUG_UNIFY`: log resolver feature unification.
    pub debug_unify: bool,
    /// `MIRVM_C2_DEBUG`: log native-archive symbol resolution.
    pub c2_debug: bool,
    /// `MIRVM_A2_DEBUG`: log dependency-image pre-key computation.
    pub a2_debug: bool,
    /// `MIRVM_PURITY_STATS`: print lowering purity statistics.
    pub purity_stats: bool,
    /// `MIRVM_SYSCALL_TRACE`: trace guest syscalls.
    pub syscall_trace: bool,
    /// `MIRVM_SEGV_DUMP`: print the fault RIP on SIGSEGV.
    pub segv_dump: bool,
}

static OPTIONS: LazyLock<Options> = LazyLock::new(Options::load);

/// This process's resolved options.
pub fn get() -> &'static Options {
    &OPTIONS
}

impl Options {
    fn load() -> Self {
        let home = raw("home")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").expect("HOME is not set");
                PathBuf::from(home).join(".mirvm")
            });
        let target_dir = raw("target_dir")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::store::TARGET.dir_in(&home).join("mirvm"));
        Self {
            home,
            target_dir,
            sysroot: raw("sysroot")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            jit_threshold: raw("jit_threshold")
                .and_then(|value| value.parse().ok())
                .filter(|&threshold| threshold > 0)
                .unwrap_or(1000),
            jit_sync: flag("jit_sync"),
            jit_stats: flag("jit_stats"),
            cargo_locked: flag("cargo_locked"),
            timing: flag("timing"),
            no_ir_cache: nonempty("no_ir_cache"),
            no_base_image: nonempty("no_base_image"),
            no_deps_image: nonempty("no_deps_image"),
            pack_no_mc: flag("pack_no_mc"),
            deps_audit_keep: flag("deps_audit_keep"),
            encoded_rustflags_append: raw("encoded_rustflags_append")
                .filter(|value| !value.is_empty()),
            jit_debug: flag("jit_debug"),
            jit_debug_dump: flag("jit_debug_dump"),
            debug_bldrs: flag("debug_bldrs"),
            debug_unify: flag("debug_unify"),
            c2_debug: flag("c2_debug"),
            a2_debug: flag("a2_debug"),
            purity_stats: nonempty("purity_stats"),
            syscall_trace: flag("syscall_trace"),
            segv_dump: flag("segv_dump"),
        }
    }

    /// `MIRVM_JIT` / `--jit`. Absent means on.
    pub fn jit(&self) -> bool {
        match raw("jit") {
            Some(value) => !(value == "off" || value == "0"),
            None => true,
        }
    }

    /// `MIRVM_STACK_SIZE` / `--stack-size`, unparsed: the caller owns the diagnostic. An empty value
    /// is returned as-is so the parser can reject it, matching the pre-register behavior.
    pub fn stack_size(&self) -> Option<String> {
        raw("stack_size")
    }

    /// `MIRVM_OFFLINE`.
    pub fn offline(&self) -> bool {
        flag("offline")
    }

    /// `MIRVM_DEPS`.
    pub fn deps(&self) -> Result<DepsTrack, String> {
        match raw("deps").as_deref() {
            None | Some("self") => Ok(DepsTrack::Own),
            Some("cargo") => Ok(DepsTrack::Cargo),
            Some(other) => Err(format!(
                "mirvm: MIRVM_DEPS only accepts `cargo` or `self` (got `{other}`)"
            )),
        }
    }

    /// `MIRVM_OUTPUT` / `--json`.
    pub fn output_format(&self) -> Result<OutputFormat, String> {
        match raw("output_format").as_deref() {
            None | Some("") | Some("text") => Ok(OutputFormat::Text),
            Some("json") => Ok(OutputFormat::Json),
            Some(other) => Err(format!(
                "mirvm: MIRVM_OUTPUT only accepts `text` or `json` (got `{other}`)"
            )),
        }
    }

    /// `MIRVM_CLESS_JOBS`, defaulting to the available parallelism.
    pub fn cless_jobs(&self) -> Result<usize, String> {
        match raw("cless_jobs") {
            None => Ok(std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)),
            Some(raw) => match raw.parse::<usize>() {
                Ok(n) if n >= 1 => Ok(n),
                _ => Err(format!(
                    "mirvm: MIRVM_CLESS_JOBS={raw} invalid (must be a positive integer)"
                )),
            },
        }
    }

    /// The `-Zthreads=` argument for one compiler session, from `MIRVM_THREADS`.
    ///
    /// `off`/unset maps to `-Zthreads=1`: on the pinned toolchain that parses back to "no thread
    /// pool", so it is a no-op today, but it fixes this session's thread count regardless of rustc's
    /// own default, which upstream is moving to two frontend threads.
    pub fn threads_arg(&self) -> Result<String, String> {
        threads_from(raw("threads").as_deref())
    }
}

/// The `MIRVM_THREADS` mapping, split out so the table can be exercised without touching the
/// process environment.
fn threads_from(raw: Option<&str>) -> Result<String, String> {
    const SEQUENTIAL: &str = "-Zthreads=1";
    match raw.map(str::trim) {
        None | Some("") | Some("off") => Ok(SEQUENTIAL.to_string()),
        // rustc's own spelling for "one thread, but keep the compiler thread-safe".
        Some("sync") => Ok("-Zthreads=sync".to_string()),
        Some(value) => match value.parse::<usize>() {
            // `0` keeps rustc's meaning (one thread per available core); 256 is rustc's ceiling,
            // and a larger value would be clamped there silently, so reject it here.
            Ok(n) if n <= 256 => Ok(format!("-Zthreads={n}")),
            _ => Err(format!(
                "mirvm: MIRVM_THREADS only accepts `off`, `sync` or an integer in 0..=256 (got `{value}`)"
            )),
        },
    }
}

// ===== internal protocol =====

/// Variables one mirvm process sets for another. Declared here so each name exists once in the
/// tree; the register rows above point at these constants.
pub mod protocol {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub const CARGO_SESSION: &str = "MIRVM_CARGO_SESSION";
    pub const CARGO_COMPILER: &str = "MIRVM_CARGO_COMPILER";
    pub const PACK: &str = "MIRVM_PACK";
    pub const GUEST_CWD: &str = "MIRVM_GUEST_CWD";
    pub const CALLER_SYSROOT: &str = "MIRVM_CALLER_SYSROOT";
    pub const CALLER_SYSROOT_PRESENT: &str = "MIRVM_CALLER_SYSROOT_PRESENT";
    pub const DOCTEST_BUILDER: &str = "MIRVM_DOCTEST_BUILDER";
    pub const DOCTEST_RUN_DIR: &str = "MIRVM_DOCTEST_RUN_DIR";

    pub fn cargo_session() -> bool {
        std::env::var_os(CARGO_SESSION).is_some()
    }

    pub fn cargo_compiler() -> bool {
        std::env::var_os(CARGO_COMPILER).is_some()
    }

    pub fn pack() -> Option<PathBuf> {
        std::env::var_os(PACK).map(PathBuf::from)
    }

    pub fn guest_cwd() -> Option<PathBuf> {
        std::env::var_os(GUEST_CWD).map(PathBuf::from)
    }

    pub fn caller_sysroot() -> Option<OsString> {
        std::env::var_os(CALLER_SYSROOT)
    }

    pub fn caller_sysroot_present() -> bool {
        std::env::var_os(CALLER_SYSROOT_PRESENT).is_some_and(|value| value == "1")
    }

    pub fn doctest_builder() -> Option<String> {
        std::env::var(DOCTEST_BUILDER).ok()
    }

    pub fn doctest_run_dir() -> Option<PathBuf> {
        std::env::var_os(DOCTEST_RUN_DIR).map(PathBuf::from)
    }

    pub fn set_cargo_session(cmd: &mut Command) {
        cmd.env(CARGO_SESSION, "1");
    }

    pub fn set_cargo_compiler(cmd: &mut Command) {
        cmd.env(CARGO_COMPILER, "1");
    }

    pub fn set_pack(cmd: &mut Command, out: &Path) {
        cmd.env(PACK, out);
    }

    pub fn set_guest_cwd(cmd: &mut Command, cwd: &Path) {
        cmd.env(GUEST_CWD, cwd);
    }

    pub fn set_caller_sysroot_present(cmd: &mut Command, present: bool) {
        cmd.env(CALLER_SYSROOT_PRESENT, if present { "1" } else { "0" });
    }

    pub fn set_caller_sysroot(cmd: &mut Command, sysroot: &Path) {
        cmd.env(CALLER_SYSROOT, sysroot);
    }

    pub fn clear_caller_sysroot(cmd: &mut Command) {
        cmd.env_remove(CALLER_SYSROOT);
    }

    pub fn set_doctest_builder(cmd: &mut Command, builder: &Path) {
        cmd.env(DOCTEST_BUILDER, builder);
    }

    pub fn set_doctest_run_dir(cmd: &mut Command, dir: &Path) {
        cmd.env(DOCTEST_RUN_DIR, dir);
    }
}

// ===== build-time constants =====

/// Injected by `build.rs` through `cargo:rustc-env`. Compile-time constants, not runtime inputs:
/// they cannot be changed after the binary is built, so they are documented here rather than in the
/// register.
pub mod build {
    /// FNV-1a over the `src/` tree plus `Cargo.lock` and `build.rs`; keys every cache layer.
    pub const BUILD_ID: &str = env!("MIRVM_BUILD_ID");
    /// `rustc --print sysroot` at build time; also the rpath target.
    pub const DEFAULT_SYSROOT: &str = env!("MIRVM_DEFAULT_SYSROOT");
    /// The build target triple, used as both host and target.
    pub const HOST: &str = env!("MIRVM_HOST");
}

// ===== rendering =====

/// The `ENV:` block of `USAGE`, generated so it cannot drift from the register.
pub fn usage_env_section() -> String {
    let mut out = String::from("ENV:\n");
    for entry in entries() {
        if entry.identity != Identity::User {
            continue;
        }
        let Some(env) = entry.env else {
            continue;
        };
        let default = if entry.default == "off" {
            String::new()
        } else {
            format!(" (default {})", entry.default)
        };
        out.push_str(&format!("    {env:<20}{}{default}\n", entry.doc));
    }
    out
}

/// The `OPTIONS:` block of `USAGE` for one subcommand, generated from the register.
pub fn usage_options_section(scope: Scope) -> String {
    let mut out = String::from("OPTIONS:\n");
    for entry in entries() {
        if !entry.scopes.contains(&scope) {
            continue;
        }
        let Some(cli) = entry.cli else {
            continue;
        };
        let mut spelling = cli.to_string();
        for alias in &entry.aliases {
            spelling.push_str(", ");
            spelling.push_str(alias);
        }
        if entry.default != "off" {
            spelling.push_str(" <VALUE>");
        }
        out.push_str(&format!("    {spelling:<22}{}\n", entry.doc));
    }
    out
}

/// The `DEV:` block of `USAGE`: diagnostic knobs, which are environment-only and deliberately kept
/// out of the option list.
pub fn usage_dev_section() -> String {
    let mut out = String::from("DEV:\n");
    for entry in entries() {
        if entry.identity != Identity::Dev {
            continue;
        }
        let Some(env) = entry.env else {
            continue;
        };
        out.push_str(&format!("    {env:<20}{}\n", entry.doc));
    }
    out
}

/// One rendered row of `mirvm options`.
struct Row {
    field: &'static str,
    identity: &'static str,
    env: String,
    cli: String,
    value: String,
    source: &'static str,
}

fn rows() -> Vec<Row> {
    entries()
        .iter()
        .map(|entry| {
            let mut cli = entry.cli.unwrap_or("-").to_string();
            for alias in &entry.aliases {
                cli.push(' ');
                cli.push_str(alias);
            }
            let value = match entry.env {
                Some(_) => raw(entry.field).unwrap_or_else(|| entry.default.to_string()),
                None => entry.default.to_string(),
            };
            Row {
                field: entry.field,
                identity: entry.identity.name(),
                env: entry.env.unwrap_or("-").to_string(),
                cli,
                value,
                source: if entry.env.is_some() {
                    source(entry.field).name()
                } else {
                    "-"
                },
            }
        })
        .collect()
}

/// `mirvm options`: every registered input with its value and where that value came from.
pub fn render() -> String {
    let rows = rows();
    let mut out = format!(
        "{:<28} {:<9} {:<32} {:<26} {:<6} {}\n",
        "FIELD", "IDENTITY", "ENV", "CLI", "SOURCE", "VALUE"
    );
    for row in &rows {
        out.push_str(&format!(
            "{:<28} {:<9} {:<32} {:<26} {:<6} {}\n",
            row.field, row.identity, row.env, row.cli, row.source, row.value
        ));
    }
    out
}

/// `mirvm options --json`. Hand-rolled so this module keeps no dependency beyond `std`: the tsan
/// harness compiles it source-for-source alongside `src/vm`.
pub fn render_json() -> String {
    let mut out = String::from("[");
    for (index, row) in rows().iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('\n');
        out.push_str(&format!(
            "  {{\"field\": {}, \"identity\": {}, \"env\": {}, \"cli\": {}, \"value\": {}, \"source\": {}}}",
            json_string(row.field),
            json_string(row.identity),
            json_string(&row.env),
            json_string(&row.cli),
            json_string(&row.value),
            json_string(row.source),
        ));
    }
    if !out.ends_with('[') {
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

/// A JSON string literal: the two mandatory escapes plus the control range.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Version stamp for `mirvm options`, so a report names the binary that produced it.
pub fn version() -> String {
    format!(
        "mirvm {} build {} host {}",
        env!("CARGO_PKG_VERSION"),
        build::BUILD_ID,
        build::HOST
    )
}

// ===== tests =====

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn register_fields_are_unique() {
        let mut seen = HashSet::new();
        for entry in entries() {
            assert!(
                seen.insert(entry.field),
                "duplicate register field `{}`",
                entry.field
            );
        }
    }

    #[test]
    fn register_env_names_are_unique() {
        let mut seen = HashSet::new();
        for entry in entries() {
            if let Some(env) = entry.env {
                assert!(seen.insert(env), "duplicate environment name `{env}`");
            }
        }
    }

    #[test]
    fn every_entry_is_documented_and_defaulted() {
        for entry in entries() {
            assert!(
                !entry.doc.is_empty(),
                "register entry `{}` has no documentation",
                entry.field
            );
            assert!(
                !entry.default.is_empty(),
                "register entry `{}` has no default description",
                entry.field
            );
        }
    }

    #[test]
    fn cli_spellings_declare_a_scope() {
        for entry in entries() {
            if entry.cli.is_some() {
                assert!(
                    !entry.scopes.is_empty(),
                    "register entry `{}` has a CLI spelling but no scope",
                    entry.field
                );
            }
        }
    }

    #[test]
    fn usage_env_section_covers_every_user_environment_variable() {
        let text = usage_env_section();
        for entry in entries() {
            let Some(env) = entry.env else { continue };
            if entry.identity != Identity::User {
                continue;
            }
            assert!(text.contains(env), "`USAGE` is missing {env}");
        }
    }

    #[test]
    fn usage_options_section_covers_every_flag_of_its_scope() {
        for scope in [
            Scope::Run,
            Scope::Test,
            Scope::Pack,
            Scope::Capture,
            Scope::Cache,
            Scope::Log,
            Scope::Deps,
            Scope::Options,
            Scope::Internal,
        ] {
            let text = usage_options_section(scope);
            for entry in entries() {
                if entry.scopes.contains(&scope)
                    && let Some(cli) = entry.cli
                {
                    assert!(
                        text.contains(cli),
                        "`USAGE` for {} is missing {cli}",
                        scope.name()
                    );
                }
            }
        }
    }

    #[test]
    fn resolved_options_read_registered_fields() {
        // Every field funnels through `lookup`, which panics on an unregistered field; touching them
        // here turns a typo into a test failure rather than a silent fallback.
        let options = get();
        let _ = &options.home;
        let _ = &options.target_dir;
        let _ = &options.sysroot;
        let _ = options.jit();
        let _ = options.jit_threshold;
        let _ = options.jit_sync;
        let _ = options.jit_stats;
        let _ = options.cargo_locked;
        let _ = options.deps();
        let _ = options.cless_jobs();
        let _ = options.threads_arg();
        let _ = options.timing;
        let _ = options.no_ir_cache;
        let _ = options.no_base_image;
        let _ = options.no_deps_image;
        let _ = options.offline();
        let _ = options.pack_no_mc;
        let _ = options.jit_debug;
        let _ = options.jit_debug_dump;
        let _ = options.debug_bldrs;
        let _ = options.debug_unify;
        let _ = options.deps_audit_keep;
        let _ = options.c2_debug;
        let _ = options.a2_debug;
        let _ = options.purity_stats;
        let _ = options.syscall_trace;
        let _ = options.segv_dump;
        let _ = &options.encoded_rustflags_append;
        let _ = options.stack_size();
    }

    #[test]
    fn threads_argument_maps_knob_values_and_rejects_typos() {
        for (raw, expected) in [
            (None, "-Zthreads=1"),
            (Some(""), "-Zthreads=1"),
            (Some("off"), "-Zthreads=1"),
            (Some("1"), "-Zthreads=1"),
            (Some("2"), "-Zthreads=2"),
            (Some("256"), "-Zthreads=256"),
            (Some("0"), "-Zthreads=0"),
            (Some("sync"), "-Zthreads=sync"),
        ] {
            assert_eq!(threads_from(raw).unwrap(), expected, "raw={raw:?}");
        }
        for raw in ["257", "-1", "abc", "2.5", "8x", "on", "true", "Off"] {
            assert!(threads_from(Some(raw)).is_err(), "raw={raw:?} must reject");
        }
    }
}
