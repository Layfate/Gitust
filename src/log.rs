//! A tiny append-only log file. Release builds have no console, so without
//! this a panic or an unexpected error would vanish without a trace.
//!
//! `gitust.log` lives in the data folder and rolls over to `gitust.old.log`
//! at 1 MB. It records errors and panics, never tokens or file contents.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 1024 * 1024;

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// Opens (or creates) the log in `dir` and installs the panic hook.
pub fn init(dir: &Path) {
    let path = dir.join("gitust.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join("gitust.old.log"));
    }
    let file =
        std::fs::create_dir_all(dir).ok().and_then(|_| OpenOptions::new().create(true).append(true).open(&path).ok());
    let _ = PATH.set(path);
    let _ = LOG.set(Mutex::new(file));

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        write("PANIC", &format!("thread '{}' {info}\n{backtrace}", thread.name().unwrap_or("?")));
        default_hook(info);
    }));
}

pub fn info(msg: &str) {
    write("INFO", msg);
}

pub fn error(msg: &str) {
    write("ERROR", msg);
}

fn write(level: &str, msg: &str) {
    let line = format!("{} [{level}] {}\n", crate::util::datetime(crate::util::now(), 0), msg.trim_end());
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    let Some(lock) = LOG.get() else { return };
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = guard.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_rolls_over() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gitust.log"), vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        init(dir.path());
        error("something broke");
        let old = std::fs::metadata(dir.path().join("gitust.old.log")).unwrap().len();
        assert_eq!(old, MAX_BYTES + 1);
        let text = std::fs::read_to_string(dir.path().join("gitust.log")).unwrap();
        assert!(text.contains("[ERROR] something broke"), "{text}");
    }
}
