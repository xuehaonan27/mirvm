#[macro_export]
macro_rules! mirvm_log {
    // Write directly to stdout / stderr with I/O error swallowed (avoid EPIPE panic)
    (stdout, $($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout().lock(), $($arg)*);
    }};
    (stderr, $($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};

    // Log levels
    ([error], $($arg:tt)*) => { $crate::mirvm_log!(@level ::log::Level::Error, $($arg)*) };
    ([warn],  $($arg:tt)*) => { $crate::mirvm_log!(@level ::log::Level::Warn,  $($arg)*) };
    ([info],  $($arg:tt)*) => { $crate::mirvm_log!(@level ::log::Level::Info,  $($arg)*) };
    ([debug], $($arg:tt)*) => { $crate::mirvm_log!(@level ::log::Level::Debug, $($arg)*) };
    // short-circuit for trace level
    ([trace], $($arg:tt)*) => {{
        if ::log::log_enabled!(::log::Level::Trace) {
            $crate::mirvm_log!(@level ::log::Level::Trace, $($arg)*);
        }
    }};

    // internal exit, with @ marking internal rules preventing external matching
    (@level $level:expr, $($arg:tt)*) => {{
        ::log::log!(target: module_path!(), $level, $($arg)*);
    }};
}
