use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use chrono::Local;

/// Line-oriented writer shared by all output pumps. Every write is a whole
/// line, so output of concurrent processes never interleaves mid-line.
pub struct LogWriter {
    out: Mutex<Box<dyn Write + Send>>,
}

impl LogWriter {
    pub fn stdout() -> Self {
        Self { out: Mutex::new(Box::new(io::stdout())) }
    }

    /// Opens `path` for appending, creating it and its parent directory.
    pub fn append(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { out: Mutex::new(Box::new(file)) })
    }

    /// `[2026-08-17 12:00:12][key] text`
    pub fn line(&self, key: &str, text: &str) {
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        self.write(&format!("[{ts}][{key}] {text}\n"));
    }

    /// The text as-is plus a newline. Used for per-process logfiles, which
    /// are already scoped to one process and meant to be an unmodified copy
    /// of its output.
    pub fn raw(&self, text: &str) {
        self.write(&format!("{text}\n"));
    }

    fn write(&self, s: &str) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}
