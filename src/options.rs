//! Single source of truth for every external input mirvm defines.
//!
//! Every environment variable and command-line option mirvm defines, and every test-only `MIRVM_*`
//! name, is declared once in the `entries!` table below: the row carries the variable, the identity,
//! the command-line spelling, the default, and how the option is read. The reader clause is what
//! generates the accessor, so an input has one way to be declared and one way to be read —
//! `options::<name>()` — and nothing outside this file resolves an option or reads the environment
//! for one. `USAGE` and `mirvm options` are generated from the register, and `repo-quality` fails
//! when `src/` spells an `MIRVM_*` name, or reads the options, anywhere else.
//!
//! Names that are not ours are deliberately absent: the `CARGO_*` contract mirvm consumes and
//! injects belongs to cargo, and the `mirvm test` selection grammar belongs to cargo's command line.

use std::collections::BTreeSet;
use std::env;
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
    Prepare,
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
            Scope::Prepare => "prepare",
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
            "Prepare" => Scope::Prepare,
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
    /// The command-line spelling is a presence flag and takes no value. Declared rather than
    /// inferred from `default`, so a flag whose environment value is a word (`MIRVM_OUTPUT=text`)
    /// still renders without a placeholder.
    pub flag: bool,
    /// The command line exports this option for the child processes it starts, and this one never
    /// reads it again. A row with a reader says so when its reader is a snapshot by intent.
    pub child_only: bool,
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
    ($entry:ident, flag) => {
        $entry.flag = true;
    };
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
    ($entry:ident, child_only) => {
        $entry.child_only = true;
    };
}

/// Emits one accessor from a register row's reader clause.
///
/// The clause carries both the signature and the read, so the function has no body of its own; what
/// distinguishes a snapshot reader from a live one is only what its closure does.
macro_rules! accessor {
    ($doc:literal, $field:ident, $ty:ty, $read:expr) => {
        #[doc = $doc]
        pub fn $field() -> $ty {
            let read: fn(&'static Options) -> $ty = $read;
            read(get())
        }
    };
}

/// Declares the register, one entry per line:
///
/// `identity field [env("NAME")] [cli("--flag")] [alias("-x")] [scope("Run")] default("..") => read;`
///
/// The identity is `user`, `dev`, `test` or `protocol`. A protocol entry names its variable through
/// [`protocol`], so a parent-to-child variable is spelled once in the tree. Every entry carries a doc
/// comment and a `default`; `flag` marks a command-line spelling that takes no value.
///
/// The reader clause is the only way an option becomes readable, and it is what generates the
/// accessor: `=> reads(TYPE, |o| ..)` answers the value this process resolved at first use, while
/// `=> live(TYPE, |o| ..)` evaluates the closure on every call. An option the command line overrides
/// after startup has to be `live`, because the parser settles it by exporting the very variable its
/// child processes inherit, and its readers have to see what a child sees. A row without the clause —
/// a spelling that exists only on a command line, or an internal protocol variable — generates no
/// accessor at all.
macro_rules! entries {
    ( $( #[doc = $doc:literal] $kind:ident $field:ident
         $( $attr:ident $( ( $($arg:expr),* ) )? )*
         $( => reads ( $rty:ty, $r:expr ) )? $( => live ( $lty:ty, $l:expr ) )? ; )* ) => {
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
                        flag: false,
                        child_only: false,
                        doc: $doc.trim_start(),
                    });
                    let entry = out.last_mut().expect("the entry was just pushed");
                    $( attr_of!(entry, $attr $(, $($arg),* )?); )*
                )*
                out
            })
        }

        // One accessor per row that declares a reader. The register is the only place an option can
        // be declared, so the read surface cannot drift from it.
        $(
            $( accessor!($doc, $field, $rty, $r); )?
            $( accessor!($doc, $field, $lty, $l); )?
        )*
    };
}

/// The internal argv spelling one mirvm process hands the runner it starts.
///
/// Declared here so the register row below owns the spelling the way an environment-backed row owns
/// its variable: `cli(...)` points at this constant, and `cli` re-exports it for the argument.
pub(crate) const INTERNAL_CAPTURE_DIR: &str = "--mirvm-capture-directory";

// ===== spellings the register points at =====

/// Names one mirvm process hands another: the rows below declare them like any other input and are
/// where a child reads them back, and [`protocol`] is how a parent writes one onto that child.
pub const CARGO_SESSION: &str = "MIRVM_CARGO_SESSION";
pub const CARGO_COMPILER: &str = "MIRVM_CARGO_COMPILER";
pub const PACK: &str = "MIRVM_PACK";
pub const GUEST_CWD: &str = "MIRVM_GUEST_CWD";
pub const CALLER_SYSROOT: &str = "MIRVM_CALLER_SYSROOT";
pub const DOCTEST_BUILDER: &str = "MIRVM_DOCTEST_BUILDER";
pub const DOCTEST_RUN_DIR: &str = "MIRVM_DOCTEST_RUN_DIR";
pub const BUILD_LOG: &str = "MIRVM_BUILD_LOG";

entries! {
    /// Machine output: reports as one JSON document, diagnostics as one JSON object per line.
    user output_format env("MIRVM_OUTPUT")     default("text")       cli("--json", "Run Prepare Pack Capture Cache Deps Options") flag
        => live(Result<OutputFormat, Error>, |_| read_output_format());
    /// The quietest diagnostic that still prints: error | warning | note | info | debug.
    user log_level     env("MIRVM_LOG")        default("warning")    cli("--verbose", "-v", "Run Prepare") flag
        => live(Result<crate::diag::Severity, Error>, |_| read_log_level());
    /// MIR-rich sysroot; equivalent to --sysroot. Default: build and cache one.
    user sysroot       env("MIRVM_SYSROOT")    default("auto-built") cli("--sysroot", "Run")
        => live(Option<PathBuf>, |_| read_sysroot());
    /// Guest main execution stack reservation; accepts a k/m/g suffix, range 1m..=1t.
    user stack_size    env("MIRVM_STACK_SIZE") default("1g")         cli("--stack-size", "Run")
        => live(Option<String>, |_| read_stack_size());
    /// Method-level JIT; `off` runs the pure interpreter (differential benchmark).
    user jit           env("MIRVM_JIT")        default("on")         cli("--jit", "Run")
        => live(bool, |_| read_jit());

    /// JIT compilation trigger threshold (diagnostic).
    user jit_threshold env("MIRVM_JIT_THRESHOLD")  default("1000")           => reads(u32, |o| o.jit_threshold);
    /// Compile on the enqueueing thread and fail loudly; makes a forced threshold observable.
    user jit_sync      env("MIRVM_JIT_SYNC")       default("off")            => reads(bool, |o| o.jit_sync);
    /// Print JIT helper frequency statistics at process exit.
    user jit_stats     env("MIRVM_JIT_STATS")      default("off")            => reads(bool, |o| o.jit_stats);
    /// Local store root: cache/ (deletable), data/ (expensive to lose), build/ (project space), run/ (process scratch).
    user home          env("MIRVM_HOME")           default("$HOME/.mirvm")   => reads(&'static std::path::Path, |o| &o.home);
    /// Relocate the Cargo track's target directory (the shared dependency store) out of `build/`.
    user target_dir    env("MIRVM_TARGET_DIR")     default("$MIRVM_HOME/build/target/mirvm") => reads(&'static std::path::Path, |o| &o.target_dir);
    /// Build frontmatter/script projects with --locked.
    user cargo_locked  env("MIRVM_CARGO_LOCKED")   default("off")            => reads(bool, |o| o.cargo_locked);
    /// `self` = zero-cargo own scheduling; `cargo` = the Cargo compatibility track.
    user deps          env("MIRVM_DEPS")           default("self")           => live(Result<DepsTrack, Error>, |_| read_deps());
    /// Cargoless compilation concurrency; =1 is the serial differential anchor.
    user cless_jobs    env("MIRVM_CLESS_JOBS")     default("available parallelism") => live(Result<usize, Error>, |_| read_cless_jobs());
    /// rustc frontend threads for the compile session: off | sync | 0..=256.
    user threads       env("MIRVM_THREADS")        default("off")            => live(Result<String, Error>, |_| read_threads());
    /// Write the phase ledger (frontend/lower/engine/total) to stderr.
    user timing        env("MIRVM_TIMING")         default("off")            => reads(bool, |o| o.timing);
    /// Bypass the L2 engine-IR cache (read and write).
    user no_ir_cache   env("MIRVM_NO_IR_CACHE")    default("off")            => reads(bool, |o| o.no_ir_cache);
    /// Bypass the pre-lowered std base image (full cold lowering).
    user no_base_image env("MIRVM_NO_BASE_IMAGE")  default("off") child_only => reads(bool, |o| o.no_base_image);
    /// Bypass the dependency image.
    user no_deps_image env("MIRVM_NO_DEPS_IMAGE")  default("off") child_only => reads(bool, |o| o.no_deps_image);
    /// Bypass the JIT code store: compile every function instead of reusing a stored entry.
    user no_jit_cache  env("MIRVM_NO_JIT_CACHE")   default("off") => reads(bool, |o| o.no_jit_cache);
    /// Disable registry HTTP; resolve from the local cache only and fail loudly on a miss.
    user offline       env("MIRVM_OFFLINE")        default("off")            => live(bool, |_| read_offline());
    /// `mirvm pack`: carry machine code out of line and rematerialize it on load.
    user pack_no_mc    env("MIRVM_PACK_NO_MC")     default("off")            => reads(bool, |o| o.pack_no_mc);

    /// Log the JIT compiler thread's receive/publish flow.
    dev  jit_debug       env("MIRVM_JIT_DEBUG")       default("off") => reads(bool, |o| o.jit_debug);
    /// Dump CLIF for functions whose compilation fails.
    dev  jit_debug_dump  env("MIRVM_JIT_DEBUG_DUMP")  default("off") => reads(bool, |o| o.jit_debug_dump);
    /// Publish what each compiled function's artifact links back to, not the module's own code.
    dev  jit_reload      env("MIRVM_JIT_RELOAD")      default("off") => reads(bool, |o| o.jit_reload);
    /// Measure each compiled or linked function (body bytes, code bytes, micros) and print the ledger at exit.
    dev  jit_ledger      env("MIRVM_JIT_LEDGER")      default("off") => reads(bool, |o| o.jit_ledger);
    /// Log build-script scheduling.
    dev  debug_bldrs     env("MIRVM_DEBUG_BLDRS")     default("off") => reads(bool, |o| o.debug_bldrs);
    /// Log resolver feature unification.
    dev  debug_unify     env("MIRVM_DEBUG_UNIFY")     default("off") => reads(bool, |o| o.debug_unify);
    /// `mirvm deps audit`: keep the scratch tree instead of cleaning it up.
    dev  deps_audit_keep env("MIRVM_DEPS_AUDIT_KEEP") default("off") => reads(bool, |o| o.deps_audit_keep);
    /// Log native-archive symbol resolution.
    dev  c2_debug        env("MIRVM_C2_DEBUG")        default("off") => reads(bool, |o| o.c2_debug);
    /// Log dependency-image pre-key computation.
    dev  a2_debug        env("MIRVM_A2_DEBUG")        default("off") => reads(bool, |o| o.a2_debug);
    /// Print lowering purity statistics.
    dev  purity_stats    env("MIRVM_PURITY_STATS")    default("off") => reads(bool, |o| o.purity_stats);
    /// Print the dedup probe: canonical fragment ids and the frozen share of a lowered layer.
    dev  frag_stats      env("MIRVM_FRAG_STATS")      default("off") => reads(bool, |o| o.frag_stats);
    /// Trace guest syscalls.
    dev  syscall_trace   env("MIRVM_SYSCALL_TRACE")   default("off") => reads(bool, |o| o.syscall_trace);
    /// Print the fault RIP on SIGSEGV to locate a JIT code crash site.
    dev  segv_dump       env("MIRVM_SEGV_DUMP")       default("off") => reads(bool, |o| o.segv_dump);

    /// Encoded rustflags appended inside the compiler wrapper; set by the differential suite.
    test encoded_rustflags_append env("MIRVM_ENCODED_RUSTFLAGS_APPEND") default("empty")
        => reads(Option<&'static str>, |o| o.encoded_rustflags_append.as_deref());

    /// Route argv into the Cargo wrapper phase.
    protocol cargo_session   env(CARGO_SESSION)   default("unset") => live(bool, |_| env::var_os(CARGO_SESSION).is_some());
    /// This session occupies Cargo's RUSTC slot rather than the wrapper slot.
    protocol cargo_compiler  env(CARGO_COMPILER)  default("unset") => live(bool, |_| env::var_os(CARGO_COMPILER).is_some());
    /// Emit a .mirvm package to this path instead of executing.
    protocol pack            env(PACK)            default("unset") => live(Option<PathBuf>, |_| env::var_os(PACK).map(PathBuf::from));
    /// Caller directory to enter before guest execution.
    protocol guest_cwd       env(GUEST_CWD)       default("unset") => live(Option<PathBuf>, |_| env::var_os(GUEST_CWD).map(PathBuf::from));
    /// The caller's own sysroot, echoed back into the guest environment.
    protocol caller_sysroot  env(CALLER_SYSROOT)  default("unset") => live(Option<std::ffi::OsString>, |_| env::var_os(CALLER_SYSROOT));
    /// Doctest builder launcher path.
    protocol doctest_builder env(DOCTEST_BUILDER) default("unset") => live(Option<String>, |_| env::var(DOCTEST_BUILDER).ok());
    /// Doctest working directory.
    protocol doctest_run_dir env(DOCTEST_RUN_DIR) default("unset") => live(Option<PathBuf>, |_| env::var_os(DOCTEST_RUN_DIR).map(PathBuf::from));
    /// `hold` = a build's own compiler diagnostics wait for its outcome; `live` = they print at once.
    protocol build_log       env(BUILD_LOG)       default("live")  => live(Option<String>, |_| env::var(BUILD_LOG).ok());

    /// Print the entry function's MIR and exit.
    user dump_mir            cli("--dump-mir", "Run")              default("off") flag;
    /// Edition for the single-file form.
    user edition             cli("--edition", "Run")               default("2024");
    /// Compatibility spelling; `vm` is the only engine and is accepted silently.
    user run_engine          cli("--engine", "Run")                default("vm");
    /// Call an exported function directly, e.g. 'fib(25)', instead of the main startup chain.
    user vm_call             cli("--vm-call", "Run")               default("unset");
    /// Print Trap-debt statistics and exit.
    user vm_stats            cli("--vm-stats", "Run")              default("off") flag;
    /// Cargo `--bin` semantics; project form only.
    user bin                 cli("--bin", "Run")                   default("unset");
    /// Ignore package.rust-version, with Cargo's semantics.
    user ignore_rust_version cli("--ignore-rust-version", "Run")   default("off") flag;
    /// Output path: the .mirvm package (`pack`) or the capture directory (`capture`).
    user output              cli("--output", "-o", "Pack Capture") default("unset");
    /// Report what `cache purge` would remove without removing it.
    user cache_dry_run       cli("--dry-run", "Cache")             default("off") flag;
    /// Purge the whole dependency-image family.
    user cache_deps          cli("--deps", "Cache")                default("off") flag;
    /// Purge the whole base-image family.
    user cache_base          cli("--base", "Cache")                default("off") flag;
    /// Purge the whole L2 engine-IR family.
    user cache_ir            cli("--ir", "Cache")                  default("off") flag;
    /// Purge scripts/ (materialized frontmatter projects).
    user cache_scripts       cli("--scripts", "Cache")             default("off") flag;
    /// Purge the unified target directory (the shared dependency store).
    user cache_target        cli("--target", "Cache")              default("off") flag;
    /// Purge all of cache/, build/ and run/: everything that costs no network to rebuild.
    user cache_all           cli("--all", "Cache")                 default("off") flag;
    /// Also purge data/ (crate store and sysroot); with --all this is a full cold start.
    user cache_data          cli("--data", "Cache")                default("off") flag;
    /// `log export` filter: engine id, or `unknown`.
    user log_engine          cli("--engine", "Log")                default("unset");
    /// `log export` filter: producer id.
    user log_producer        cli("--producer", "Log")              default("unset");
    /// `log export` filter: thread id.
    user log_tid             cli("--tid", "Log")                   default("unset");
    /// `log export` filter: event kind.
    user log_kind            cli("--kind", "Log")                  default("unset");
    /// `log export` filter: a sequence number or a START:END range.
    user log_sequence        cli("--sequence", "Log")              default("unset");
    /// Internal: forwarded capture directory for the Cargo runner form.
    user mirvm_capture_dir   cli(INTERNAL_CAPTURE_DIR, "Internal") default("unset");
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

/// A rejected value of an environment-backed input.
///
/// The variable's name is carried as a field read from the register, never written into the message,
/// because a second spelling of an `MIRVM_*` name is what `check_option_register` forbids.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("{env} only accepts `cargo` or `self` (got `{value}`)")]
    DepsTrack { env: &'static str, value: String },
    #[error("{env} only accepts `text` or `json` (got `{value}`)")]
    OutputFormat { env: &'static str, value: String },
    #[error("{env}={value} invalid (must be a positive integer)")]
    ClessJobs { env: &'static str, value: String },
    #[error("{env} only accepts `off`, `sync` or an integer in 0..=256 (got `{value}`)")]
    Threads { env: &'static str, value: String },
    #[error(
        "{env} only accepts a severity name: error, warning, note, info or debug (got `{value}`)"
    )]
    LogLevel { env: &'static str, value: String },
}

crate::diag_codes! {
    Error: Options => {
        DepsTrack => "options.deps_track" Usage,
        OutputFormat => "options.output_format" Usage,
        ClessJobs => "options.cless_jobs" Usage,
        Threads => "options.threads" Usage,
        LogLevel => "options.log_level" Usage,
    }
}

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
/// Private to this module: the register rows are the one surface that reads it, so an option has one
/// place to state its default, how it is read, and how a call site spells it.
struct Options {
    home: PathBuf,
    target_dir: PathBuf,
    jit_threshold: u32,
    jit_sync: bool,
    jit_stats: bool,
    cargo_locked: bool,
    timing: bool,
    no_ir_cache: bool,
    no_base_image: bool,
    no_deps_image: bool,
    no_jit_cache: bool,
    pack_no_mc: bool,
    deps_audit_keep: bool,
    encoded_rustflags_append: Option<String>,
    jit_debug: bool,
    jit_debug_dump: bool,
    jit_reload: bool,
    jit_ledger: bool,
    debug_bldrs: bool,
    debug_unify: bool,
    c2_debug: bool,
    a2_debug: bool,
    purity_stats: bool,
    frag_stats: bool,
    syscall_trace: bool,
    segv_dump: bool,
}

static OPTIONS: LazyLock<Options> = LazyLock::new(Options::load);

/// This process's resolved options, the private half of the accessors below.
fn get() -> &'static Options {
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
            no_jit_cache: nonempty("no_jit_cache"),
            pack_no_mc: flag("pack_no_mc"),
            deps_audit_keep: flag("deps_audit_keep"),
            encoded_rustflags_append: raw("encoded_rustflags_append")
                .filter(|value| !value.is_empty()),
            jit_debug: flag("jit_debug"),
            jit_debug_dump: flag("jit_debug_dump"),
            jit_reload: flag("jit_reload"),
            jit_ledger: flag("jit_ledger"),
            debug_bldrs: flag("debug_bldrs"),
            debug_unify: flag("debug_unify"),
            c2_debug: flag("c2_debug"),
            a2_debug: flag("a2_debug"),
            purity_stats: nonempty("purity_stats"),
            frag_stats: nonempty("frag_stats"),
            syscall_trace: flag("syscall_trace"),
            segv_dump: flag("segv_dump"),
        }
    }
}

/// The `MIRVM_JIT` reader: absent means on.
fn read_jit() -> bool {
    match raw("jit") {
        Some(value) => !(value == "off" || value == "0"),
        None => true,
    }
}

/// The `MIRVM_STACK_SIZE` reader, unparsed: the caller owns the diagnostic. An empty value is
/// returned as-is so the parser can reject it, matching the pre-register behavior.
fn read_stack_size() -> Option<String> {
    raw("stack_size")
}

/// The `MIRVM_SYSROOT` reader: the sysroot in effect, which `--sysroot` writes after startup.
///
/// Everything that names a sysroot has to agree on it — the compiler session, the stamp, the base
/// image and the dependency fingerprint — so this is read live rather than resolved at first use.
fn read_sysroot() -> Option<PathBuf> {
    raw("sysroot")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The `MIRVM_OFFLINE` reader.
fn read_offline() -> bool {
    flag("offline")
}

/// The `MIRVM_DEPS` reader.
fn read_deps() -> Result<DepsTrack, Error> {
    match raw("deps").as_deref() {
        None | Some("self") => Ok(DepsTrack::Own),
        Some("cargo") => Ok(DepsTrack::Cargo),
        Some(other) => Err(Error::DepsTrack {
            env: env_var_name("deps"),
            value: other.to_string(),
        }),
    }
}

/// The `MIRVM_OUTPUT` mapping: `text` is the default and an unknown word is the caller's to report.
fn output_from(value: Option<&str>) -> Result<OutputFormat, Error> {
    match value {
        None | Some("") | Some("text") => Ok(OutputFormat::Text),
        Some("json") => Ok(OutputFormat::Json),
        Some(other) => Err(Error::OutputFormat {
            env: env_var_name("output_format"),
            value: other.to_string(),
        }),
    }
}

/// The `MIRVM_OUTPUT` reader.
fn read_output_format() -> Result<OutputFormat, Error> {
    output_from(raw("output_format").as_deref())
}

/// Whether `MIRVM_OUTPUT` selects machine output.
///
/// The total form, for a reader that cannot report a bad value: [`crate::diag`] renders a diagnostic
/// before the parser has seen the flag, and there an unknown word is text like any other.
pub fn machine_output() -> bool {
    matches!(
        output_from(raw("output_format").as_deref()),
        Ok(OutputFormat::Json)
    )
}

/// The `MIRVM_LOG` reader: the quietest diagnostic that still prints.
///
/// The names are [`crate::diag::Severity`]'s own vocabulary, so a severity this register accepts is
/// one the emitter can render, and an unknown word is rejected rather than silently read as the
/// default. `MIRVM_JIT_DEBUG` is the JIT's own spelling of `debug`: its traces are ordinary Debug
/// diagnostics, so the knob a command already exports keeps selecting them. An explicit
/// `MIRVM_LOG` wins, which is what makes the two spellings one setting rather than two.
fn read_log_level() -> Result<crate::diag::Severity, Error> {
    match raw("log_level").as_deref() {
        None | Some("") if crate::options::jit_debug() => Ok(crate::diag::Severity::Debug),
        None | Some("") => Ok(crate::diag::Severity::Warning),
        Some(name) => crate::diag::Severity::from_name(name).ok_or_else(|| Error::LogLevel {
            env: env_var_name("log_level"),
            value: name.to_string(),
        }),
    }
}

/// The `MIRVM_CLESS_JOBS` reader, defaulting to the available parallelism.
fn read_cless_jobs() -> Result<usize, Error> {
    match raw("cless_jobs") {
        None => Ok(std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)),
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err(Error::ClessJobs {
                env: env_var_name("cless_jobs"),
                value: raw,
            }),
        },
    }
}

/// The `MIRVM_THREADS` reader: the `-Zthreads=` argument for one compiler session.
///
/// `off`/unset maps to `-Zthreads=1`: on the pinned toolchain that parses back to "no thread pool",
/// so it is a no-op today, but it fixes this session's thread count regardless of rustc's own
/// default, which upstream is moving to two frontend threads.
fn read_threads() -> Result<String, Error> {
    threads_from(raw("threads").as_deref())
}

/// The `MIRVM_THREADS` mapping, split out so the table can be exercised without touching the
/// process environment.
fn threads_from(raw: Option<&str>) -> Result<String, Error> {
    const SEQUENTIAL: &str = "-Zthreads=1";
    match raw.map(str::trim) {
        None | Some("") | Some("off") => Ok(SEQUENTIAL.to_string()),
        // rustc's own spelling for "one thread, but keep the compiler thread-safe".
        Some("sync") => Ok("-Zthreads=sync".to_string()),
        Some(value) => match value.parse::<usize>() {
            // `0` keeps rustc's meaning (one thread per available core); 256 is rustc's ceiling,
            // and a larger value would be clamped there silently, so reject it here.
            Ok(n) if n <= 256 => Ok(format!("-Zthreads={n}")),
            _ => Err(Error::Threads {
                env: env_var_name("threads"),
                value: value.to_string(),
            }),
        },
    }
}

// ===== internal protocol =====

/// The writing half of the parent-to-child protocol.
///
/// The names live beside the register, which declares them and is where a child reads them back;
/// this module is what a parent uses to write one onto a child's `Command`, and answers the one
/// question that is about a value rather than about it being present (`build_log_hold`).
pub mod protocol {
    use std::path::Path;
    use std::process::Command;

    use super::{
        CALLER_SYSROOT, CARGO_COMPILER, CARGO_SESSION, DOCTEST_BUILDER, DOCTEST_RUN_DIR, GUEST_CWD,
        PACK,
    };

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

    /// Whether this process holds a guest build's compiler diagnostics back.
    ///
    /// The predicate view of the `build_log` row, where the variable and its read are declared; this
    /// only names the value that means "hold".
    pub fn build_log_hold() -> bool {
        super::build_log().is_some_and(|value| value == "hold")
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
        if !entry.flag {
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

/// `mirvm options --json`: the register as one versioned document, so a consumer reads the same
/// binary identity and the same rows that the text form prints. The writer comes from `diag::json`,
/// which is `std`-only for the same reason this module is: the tsan harness compiles both
/// source-for-source alongside `src/vm`.
pub fn render_json() -> String {
    let rows: Vec<String> = rows()
        .iter()
        .map(|row| {
            let mut out = crate::diag::json::Writer::new();
            out.string("field", row.field);
            out.string("identity", row.identity);
            out.string("env", &row.env);
            out.string("cli", &row.cli);
            out.string("value", &row.value);
            out.string("source", row.source);
            out.finish()
        })
        .collect();
    let mut out = crate::diag::json::Writer::document();
    out.string("version", env!("CARGO_PKG_VERSION"));
    out.string("build_id", build::BUILD_ID);
    out.string("host", build::HOST);
    out.raw("options", &crate::diag::json::array(&rows));
    out.finish()
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
        // Every accessor funnels through `lookup`, which panics on an unregistered field; touching
        // them here turns a typo into a test failure rather than a silent fallback.
        let _ = super::home();
        let _ = super::target_dir();
        let _ = super::sysroot();
        let _ = super::jit();
        let _ = super::jit_threshold();
        let _ = super::jit_sync();
        let _ = super::jit_stats();
        let _ = super::cargo_locked();
        let _ = super::deps();
        let _ = super::cless_jobs();
        let _ = super::threads();
        let _ = super::timing();
        let _ = super::no_ir_cache();
        let _ = super::no_base_image();
        let _ = super::no_deps_image();
        let _ = super::no_jit_cache();
        let _ = super::offline();
        let _ = super::pack_no_mc();
        let _ = super::jit_debug();
        let _ = super::jit_debug_dump();
        let _ = super::debug_bldrs();
        let _ = super::debug_unify();
        let _ = super::deps_audit_keep();
        let _ = super::c2_debug();
        let _ = super::a2_debug();
        let _ = super::purity_stats();
        let _ = super::frag_stats();
        let _ = super::syscall_trace();
        let _ = super::segv_dump();
        let _ = super::encoded_rustflags_append();
        let _ = super::stack_size();
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
