//! A serial port that is not one.
//!
//! The existing integration tests stand a pseudo-terminal in for the hardware,
//! which works well but is Unix-only — so on Windows the suite compiles to
//! nothing and none of it ever runs. `serialport::SerialPort` is a public trait,
//! though, so a pair of in-memory queues can wear it just as well, and then the
//! dashboard's supervisor can be tested anywhere.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

/// A scratch directory that removes itself, so tests leave nothing behind and
/// never collide when run in parallel.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "serial-tcp-{tag}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The two directions of a fake line, shared by every clone of both ends.
#[derive(Default)]
struct Line {
    /// Bytes the device has produced, waiting to be read by the port.
    from_device: Mutex<VecDeque<u8>>,
    /// Bytes written to the port, waiting to be read by the device.
    to_device: Mutex<VecDeque<u8>>,
    settings: Mutex<Settings>,
    broken: AtomicBool,
}

#[derive(Clone, Copy)]
struct Settings {
    baud: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            baud: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    }
}

/// The port half — what gets handed to the code under test in place of real
/// hardware.
pub struct FakePort {
    line: Arc<Line>,
    timeout: Duration,
}

/// The hardware half — what a test uses to play the role of the device.
pub struct FakeDevice {
    line: Arc<Line>,
}

/// A fresh line, with nothing on it.
pub fn fake_pair() -> (FakePort, FakeDevice) {
    let line = Arc::new(Line::default());
    (
        FakePort {
            line: Arc::clone(&line),
            timeout: Duration::from_millis(50),
        },
        FakeDevice { line },
    )
}

impl FakeDevice {
    /// Say something, as the hardware would.
    pub fn send(&self, bytes: &[u8]) {
        self.line
            .from_device
            .lock()
            .unwrap()
            .extend(bytes.iter().copied());
    }

    /// Everything written to the device so far, without waiting.
    pub fn received(&self) -> Vec<u8> {
        self.line.to_device.lock().unwrap().drain(..).collect()
    }

    /// Wait for at least `n` bytes to arrive, then return everything received.
    pub fn wait_for(&self, n: usize, patience: Duration) -> Vec<u8> {
        let deadline = Instant::now() + patience;
        loop {
            if self.line.to_device.lock().unwrap().len() >= n {
                return self.received();
            }
            if Instant::now() >= deadline {
                return self.received();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Make every later read and write fail, standing in for a device that has
    /// been unplugged.
    pub fn break_line(&self) {
        self.line.broken.store(true, Ordering::Relaxed);
    }
}

impl Read for FakePort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.line.broken.load(Ordering::Relaxed) {
            return Err(io::Error::other("device went away"));
        }

        let mut queue = self.line.from_device.lock().unwrap();
        if queue.is_empty() {
            drop(queue);
            // Real ports report a timeout on an idle line rather than blocking
            // forever, and the pumps rely on that to notice shutdown.
            std::thread::sleep(self.timeout.min(Duration::from_millis(20)));
            return Err(io::Error::new(io::ErrorKind::TimedOut, "idle"));
        }

        let n = buf.len().min(queue.len());
        for slot in buf.iter_mut().take(n) {
            *slot = queue.pop_front().expect("checked non-empty above");
        }
        Ok(n)
    }
}

impl Write for FakePort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.line.broken.load(Ordering::Relaxed) {
            return Err(io::Error::other("device went away"));
        }
        self.line
            .to_device
            .lock()
            .unwrap()
            .extend(buf.iter().copied());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialPort for FakePort {
    fn name(&self) -> Option<String> {
        Some("FAKE0".to_owned())
    }

    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(self.line.settings.lock().unwrap().baud)
    }

    fn data_bits(&self) -> serialport::Result<DataBits> {
        Ok(self.line.settings.lock().unwrap().data_bits)
    }

    fn flow_control(&self) -> serialport::Result<FlowControl> {
        Ok(self.line.settings.lock().unwrap().flow_control)
    }

    fn parity(&self) -> serialport::Result<Parity> {
        Ok(self.line.settings.lock().unwrap().parity)
    }

    fn stop_bits(&self) -> serialport::Result<StopBits> {
        Ok(self.line.settings.lock().unwrap().stop_bits)
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn set_baud_rate(&mut self, baud_rate: u32) -> serialport::Result<()> {
        self.line.settings.lock().unwrap().baud = baud_rate;
        Ok(())
    }

    fn set_data_bits(&mut self, data_bits: DataBits) -> serialport::Result<()> {
        self.line.settings.lock().unwrap().data_bits = data_bits;
        Ok(())
    }

    fn set_flow_control(&mut self, flow_control: FlowControl) -> serialport::Result<()> {
        self.line.settings.lock().unwrap().flow_control = flow_control;
        Ok(())
    }

    fn set_parity(&mut self, parity: Parity) -> serialport::Result<()> {
        self.line.settings.lock().unwrap().parity = parity;
        Ok(())
    }

    fn set_stop_bits(&mut self, stop_bits: StopBits) -> serialport::Result<()> {
        self.line.settings.lock().unwrap().stop_bits = stop_bits;
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.timeout = timeout;
        Ok(())
    }

    fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }

    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn bytes_to_read(&self) -> serialport::Result<u32> {
        Ok(self.line.from_device.lock().unwrap().len() as u32)
    }

    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(self.line.to_device.lock().unwrap().len() as u32)
    }

    fn clear(&self, buffer_to_clear: ClearBuffer) -> serialport::Result<()> {
        match buffer_to_clear {
            ClearBuffer::Input => self.line.from_device.lock().unwrap().clear(),
            ClearBuffer::Output => self.line.to_device.lock().unwrap().clear(),
            ClearBuffer::All => {
                self.line.from_device.lock().unwrap().clear();
                self.line.to_device.lock().unwrap().clear();
            }
        }
        Ok(())
    }

    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        Ok(Box::new(FakePort {
            line: Arc::clone(&self.line),
            timeout: self.timeout,
        }))
    }

    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }

    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}
