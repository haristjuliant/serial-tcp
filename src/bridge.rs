//! The one primitive this whole tool is built on: pump bytes between two
//! endpoints until either side goes away.

use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::endpoint::Halves;

/// How long to wait for the second direction to notice the shutdown flag before
/// giving up on it. Generous relative to [`crate::endpoint::IO_TIMEOUT`].
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Bytes transferred during one session.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub a_to_b: u64,
    pub b_to_a: u64,
}

/// Bridge two endpoints until either direction ends, then tear the other down.
///
/// `label_a` and `label_b` are only used for log messages, e.g. `"serial"` and
/// `"tcp"`.
pub fn bridge(a: Halves, b: Halves, label_a: &str, label_b: &str) -> Result<Stats> {
    let (a_read, a_write) = a;
    let (b_read, b_write) = b;

    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<(bool, u64)>();

    spawn_pump(
        a_read,
        b_write,
        Arc::clone(&shutdown),
        format!("{label_a} -> {label_b}"),
        true,
        tx.clone(),
    );
    spawn_pump(
        b_read,
        a_write,
        Arc::clone(&shutdown),
        format!("{label_b} -> {label_a}"),
        false,
        tx,
    );

    let mut stats = Stats::default();
    let mut record = |(is_a_to_b, n): (bool, u64)| {
        if is_a_to_b {
            stats.a_to_b = n;
        } else {
            stats.b_to_a = n;
        }
    };

    // Whichever direction ends first decides the session is over.
    if let Ok(first) = rx.recv() {
        record(first);
    }
    shutdown.store(true, Ordering::Relaxed);

    // The other pump should wake within one IO_TIMEOUT and report in. A stdin
    // reader never will, since it has no timeout; that thread stays parked and
    // is reaped when the process exits.
    match rx.recv_timeout(DRAIN_GRACE) {
        Ok(second) => record(second),
        Err(_) => log::debug!("a direction did not stop within {DRAIN_GRACE:?}; detaching it"),
    }

    Ok(stats)
}

fn spawn_pump(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    shutdown: Arc<AtomicBool>,
    label: String,
    is_a_to_b: bool,
    done: mpsc::Sender<(bool, u64)>,
) {
    std::thread::spawn(move || {
        let transferred = pump(reader, writer, &shutdown, &label);
        // Ending in either direction ends the session.
        shutdown.store(true, Ordering::Relaxed);
        let _ = done.send((is_a_to_b, transferred));
    });
}

fn pump(
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
    shutdown: &AtomicBool,
    label: &str,
) -> u64 {
    let mut buf = [0u8; 4096];
    let mut total: u64 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        let n = match reader.read(&mut buf) {
            Ok(0) => {
                log::debug!("{label}: peer closed");
                break;
            }
            Ok(n) => n,
            // A read timeout is the normal case when the line is idle. Serial
            // ports report TimedOut; sockets report WouldBlock on Unix and
            // TimedOut on Windows.
            Err(e) if is_idle(&e) => continue,
            Err(e) => {
                log::debug!("{label}: read failed: {e}");
                break;
            }
        };

        // Forward immediately rather than waiting for a full buffer — holding
        // bytes back to batch them would distort the timing the far end sees.
        if let Err(e) = writer.write_all(&buf[..n]).and_then(|()| writer.flush()) {
            log::debug!("{label}: write failed: {e}");
            break;
        }
        total += n as u64;
    }

    log::debug!("{label}: stopped after {total} bytes");
    total
}

/// Whether an error just means "nothing arrived yet", rather than a real fault.
fn is_idle(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
    )
}
