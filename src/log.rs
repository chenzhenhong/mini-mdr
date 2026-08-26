use std::{
    fmt::Arguments,
    io::{self, Write},
};

/// Writes a levelled message to stderr without ever panicking.
///
/// Release builds on Windows run under the GUI subsystem where the standard
/// error handle is invalid; `eprintln!` panics in that situation, so every
/// diagnostic must go through this module instead.
pub fn write(level: &str, args: Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "[{level}] {args}");
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::write("INFO", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::write("WARN", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::write("ERROR", format_args!($($arg)*)) };
}
