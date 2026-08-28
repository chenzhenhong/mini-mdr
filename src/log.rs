use std::{
    fmt::Arguments,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024; // 5 MB
const ROTATE_KEEP: u64 = 2 * 1024 * 1024; // keep last 2 MB after rotation

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

fn log_path() -> Option<PathBuf> {
    Some(
        crate::config::Config::config_dir()
            .ok()?
            .join("mini-mdr.log"),
    )
}

fn try_init() -> Option<()> {
    let path = log_path()?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    FILE.set(Mutex::new(file)).ok();
    Some(())
}

pub fn init() {
    if try_init().is_none() {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "[WARN] could not open log file, logging to stderr only"
        );
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;

    let mut y = 1970i64;
    let mut day_count = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if day_count < days_in_year {
            break;
        }
        day_count -= days_in_year;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 1u32;
    for &md in &month_days {
        if day_count < md {
            break;
        }
        day_count -= md;
        m += 1;
    }
    let d = day_count + 1;
    format!("{y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}:{seconds:02}")
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn rotate_if_needed(file: &File) {
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() <= MAX_LOG_SIZE {
        return;
    }
    let path = match log_path() {
        Some(p) => p,
        None => return,
    };
    let content = fs::read_to_string(&path).unwrap_or_default();
    let keep_bytes = ROTATE_KEEP as usize;
    let start = if content.len() > keep_bytes {
        content[content.len() - keep_bytes..]
            .find('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    } else {
        0
    };
    let trimmed = &content[start..];
    let _ = fs::write(&path, trimmed);
    if let Ok(new_file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = FILE.set(Mutex::new(new_file));
    }
}

/// Writes a levelled message to stderr and the log file without ever panicking.
///
/// Release builds on Windows run under the GUI subsystem where the standard
/// error handle is invalid; `eprintln!` panics in that situation, so every
/// diagnostic must go through this module instead.
pub fn write(level: &str, args: Arguments<'_>) {
    let ts = timestamp();
    let line = format!("[{ts}] [{level}] {args}\n");

    // stderr (best-effort)
    {
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(line.as_bytes());
    }

    // file
    if let Some(guard) = FILE.get()
        && let Ok(mut file) = guard.lock()
    {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
        rotate_if_needed(&file);
    }
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
