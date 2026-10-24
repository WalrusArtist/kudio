//! Developer log, enabled with `--log[=PATH]`.
//!
//! Deliberately tiny: one global, line-buffered file behind a `OnceLock`.
//! When the flag is off, [`dlog!`] costs a single "is it initialised?"
//! check and never formats anything. Only the UI thread writes to it; the
//! audio thread must never call it.

use std::fmt;
use std::fs::File;
use std::io::{LineWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Log {
    out: Mutex<LineWriter<File>>,
    start: Instant,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Open (truncate) the log file. Call once at startup.
pub fn init(path: &Path) -> std::io::Result<()> {
    let file = File::create(path)?;
    let _ = LOG.set(Log {
        out: Mutex::new(LineWriter::new(file)),
        start: Instant::now(),
    });
    Ok(())
}

#[inline]
pub fn enabled() -> bool {
    LOG.get().is_some()
}

pub fn write(args: fmt::Arguments) {
    if let Some(log) = LOG.get()
        && let Ok(mut out) = log.out.lock()
    {
        let t = log.start.elapsed().as_secs_f64();
        let _ = writeln!(out, "[{t:10.4}] {args}");
    }
}

pub fn flush() {
    if let Some(log) = LOG.get()
        && let Ok(mut out) = log.out.lock()
    {
        let _ = out.flush();
    }
}

/// `dlog!("format {}", args)` — writes to the developer log if `--log` was given.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        if $crate::devlog::enabled() {
            $crate::devlog::write(format_args!($($arg)*));
        }
    };
}
