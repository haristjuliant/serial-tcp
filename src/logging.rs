//! Console logging plus an always-on debug-level file log.
//!
//! The console respects `--verbose` like before, but a session that looked
//! clean on screen (info level) is often exactly the one you need to debug
//! after the fact — the file exists so nothing has to be reproduced with
//! `--verbose` in hand.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::{LevelFilter, Log, Metadata, Record};

/// Install the combined logger. `log_file` is where the full debug trace goes;
/// pass `None` to skip file logging entirely (console-only, old behavior).
pub fn init(verbose: bool, log_file: Option<&Path>) -> Result<()> {
    let console_level = if verbose { "debug" } else { "info" };
    let console =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(console_level))
            .format_timestamp_millis()
            .build();

    let file = log_file.map(FileLogger::open).transpose()?;

    // The global filter is the first gate every record passes through, before
    // either sink sees it — it must stay at the widest level either sink
    // wants, or the file would silently miss debug records the console
    // filtered out upstream.
    log::set_max_level(LevelFilter::Debug);
    log::set_boxed_logger(Box::new(CombinedLogger { console, file }))
        .context("failed to install logger")?;
    Ok(())
}

struct CombinedLogger {
    console: env_logger::Logger,
    file: Option<FileLogger>,
}

impl Log for CombinedLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.console.enabled(metadata) || self.file.is_some()
    }

    fn log(&self, record: &Record) {
        if self.console.matches(record) {
            self.console.log(record);
        }
        if let Some(file) = &self.file {
            file.log(record);
        }
    }

    fn flush(&self) {
        self.console.flush();
    }
}

struct FileLogger {
    writer: Mutex<std::fs::File>,
}

impl FileLogger {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        Ok(Self {
            writer: Mutex::new(file),
        })
    }

    fn log(&self, record: &Record) {
        // Every record, regardless of what the console filter kept, so a run
        // that never used --verbose can still be diagnosed from the file.
        //
        // Built into one `String` and written in a single `write_all` call:
        // multiple `write!` calls on the raw file would each be a separate
        // syscall, and another process appending to the same log (e.g. `serve`
        // and `connect` sharing a working directory) could interleave its own
        // line's syscalls in between ours.
        let line = format!(
            "{} {:5} {}: {}\n",
            timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}

/// `seconds.millis` since the Unix epoch, UTC. Not calendar-formatted — no
/// datetime dependency is otherwise needed in this crate — but sortable,
/// diffable, and matches what the bridge's own timing already assumes.
fn timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", dur.as_secs(), dur.subsec_millis())
}
