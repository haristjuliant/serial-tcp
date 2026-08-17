//! Watching bytes go past without getting in their way.
//!
//! Everything here hangs off [`Halves`](crate::endpoint::Halves), the pair of
//! boxed reader and writer the bridge already works in terms of. Wrapping those
//! means `bridge.rs` and its pumps need no knowledge of the dashboard at all.
//!
//! The hard rule is that a browser must never slow the wire down. A phone on bad
//! Wi-Fi watching the monitor is a slow consumer, and if its socket could exert
//! backpressure it would smear the inter-frame gaps that protocols like Modbus
//! RTU depend on — the very thing `TCP_NODELAY` and the unbatched pumps exist to
//! protect. So sends to subscribers are non-blocking and frames are dropped, not
//! queued, when a subscriber falls behind. The count of what was dropped is
//! surfaced to the UI so a lossy *view* is never mistaken for a lossy *link*.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::rfc2217::codec::SharedWriter;

/// How many frames a subscriber may fall behind before it starts losing them.
const SUBSCRIBER_BACKLOG: usize = 256;

/// Which way a chunk of bytes was travelling.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Device to network — what the hardware said.
    Rx,
    /// Network to device — what was sent to the hardware.
    Tx,
}

impl Dir {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rx => "rx",
            Self::Tx => "tx",
        }
    }
}

/// One chunk of traffic, as it came off a single read.
#[derive(Debug)]
pub struct Frame {
    pub dir: Dir,
    /// Milliseconds since the Unix epoch, for ordering in the UI.
    pub at_ms: u64,
    pub data: Vec<u8>,
}

/// Per-port traffic counters and the fan-out to whoever is watching.
#[derive(Default)]
pub struct Tap {
    subscribers: Mutex<Vec<Subscriber>>,
    /// Read without taking the lock, so the hot path can skip all the work when
    /// nobody is watching.
    subscriber_count: AtomicUsize,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    next_id: AtomicU64,
}

struct Subscriber {
    id: u64,
    tx: SyncSender<Arc<Frame>>,
    dropped: Arc<AtomicU64>,
}

impl Tap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn rx_bytes(&self) -> u64 {
        self.rx_bytes.load(Ordering::Relaxed)
    }

    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes.load(Ordering::Relaxed)
    }

    pub fn watchers(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Forget the byte counts, without disturbing anyone currently watching.
    pub fn reset_counters(&self) {
        self.rx_bytes.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
    }

    /// Record `data` and hand it to every watcher. Never blocks.
    pub fn publish(&self, dir: Dir, data: &[u8]) {
        let counter = match dir {
            Dir::Rx => &self.rx_bytes,
            Dir::Tx => &self.tx_bytes,
        };
        counter.fetch_add(data.len() as u64, Ordering::Relaxed);

        // The common case is nobody watching; don't allocate for it.
        if self.subscriber_count.load(Ordering::Relaxed) == 0 {
            return;
        }

        let frame = Arc::new(Frame {
            dir,
            at_ms: now_ms(),
            data: data.to_vec(),
        });

        let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subscribers.retain(|sub| match sub.tx.try_send(Arc::clone(&frame)) {
            Ok(()) => true,
            // Behind, but still there — drop this frame rather than the wire's pace.
            Err(TrySendError::Full(_)) => {
                sub.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            // The browser went away and its reader thread has exited.
            Err(TrySendError::Disconnected(_)) => false,
        });
        self.subscriber_count
            .store(subscribers.len(), Ordering::Relaxed);
    }

    /// Start watching. The subscription unregisters itself when dropped.
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let (tx, rx) = sync_channel(SUBSCRIBER_BACKLOG);
        let dropped = Arc::new(AtomicU64::new(0));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subscribers.push(Subscriber {
            id,
            tx,
            dropped: Arc::clone(&dropped),
        });
        self.subscriber_count
            .store(subscribers.len(), Ordering::Relaxed);
        drop(subscribers);

        Subscription {
            tap: Arc::clone(self),
            id,
            rx,
            dropped,
        }
    }

    fn unsubscribe(&self, id: u64) {
        let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subscribers.retain(|s| s.id != id);
        self.subscriber_count
            .store(subscribers.len(), Ordering::Relaxed);
    }
}

/// A live view of one port's traffic.
pub struct Subscription {
    tap: Arc<Tap>,
    id: u64,
    rx: Receiver<Arc<Frame>>,
    dropped: Arc<AtomicU64>,
}

impl Subscription {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Arc<Frame>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Frames this watcher was too slow to take.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.tap.unsubscribe(self.id);
    }
}

/// A reader that reports everything it reads to a [`Tap`] on the way through.
///
/// Counting on the *read* side of each direction is what makes both totals fall
/// out for free: reading from the serial port is the device talking, reading
/// from the socket is the client talking.
pub struct TapReader {
    inner: Box<dyn Read + Send>,
    tap: Arc<Tap>,
    dir: Dir,
}

impl TapReader {
    pub fn new(inner: Box<dyn Read + Send>, tap: Arc<Tap>, dir: Dir) -> Self {
        Self { inner, tap, dir }
    }
}

impl Read for TapReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.tap.publish(self.dir, &buf[..n]);
        }
        Ok(n)
    }
}

/// The way anything reaches the device.
///
/// Both the bridge's network-to-device direction and the dashboard's send box
/// write through one of these, over a single shared handle, so their bytes can
/// never interleave into a corrupt frame. Because everything funnels through
/// here, this is also the honest place to count what was sent — and it counts
/// what actually reached the wire, which for RFC 2217 is the decoded payload
/// rather than the Telnet-escaped bytes on the socket.
pub struct TapSink {
    out: SharedWriter,
    tap: Arc<Tap>,
}

impl TapSink {
    pub fn new(out: SharedWriter, tap: Arc<Tap>) -> Self {
        Self { out, tap }
    }
}

impl Write for TapSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        {
            // Poisoning here means another writer panicked mid-write. Refusing
            // to write from then on would strand the session; carrying on at
            // worst repeats the truncation that already happened.
            let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
            out.write_all(buf)?;
        }
        self.tap.publish(Dir::Tx, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        out.flush()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
