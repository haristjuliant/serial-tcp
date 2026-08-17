//! One thread per running port: accept clients, bridge them, and be stoppable.
//!
//! `server::serve_on` cannot be reused directly here because it blocks forever
//! in `listener.incoming()` and has no way to be cancelled — its only exit is the
//! `max_sessions` counter the tests use. A dashboard with a Stop button needs
//! more than that, so the accept loop is reimplemented on a non-blocking
//! listener polled at the same 50 ms cadence the rest of the codebase uses to
//! notice shutdown flags. The bridging itself still goes through
//! `server::session_with`, so raw and RFC 2217 behave identically to `serve`.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use serialport::{ClearBuffer, SerialPort};

use crate::dashboard::config::PortConfig;
use crate::dashboard::net::Allowlist;
use crate::dashboard::registry::{DeviceOpener, PortEntry, bind_listener};
use crate::dashboard::tap::{Dir, TapReader, TapSink};
use crate::endpoint::{Halves, IO_TIMEOUT};
use crate::rfc2217::codec::{SharedWriter, share};
use crate::server::{self, Options};

/// The handle the registry keeps on a running port.
pub struct Control {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// A clone of whichever client stream is currently bridged, kept so that
    /// stopping can cut it loose instead of waiting for the client to leave.
    active: Arc<Mutex<Option<TcpStream>>>,
    /// The write side of the device. Held here — not inside the thread — so the
    /// send box works whether or not a client is connected.
    inject: SharedWriter,
    pub bound: SocketAddr,
}

impl Control {
    pub fn inject(&self) -> SharedWriter {
        Arc::clone(&self.inject)
    }

    /// Stop the port and wait for its thread to finish.
    ///
    /// Shutting the live socket down is what makes this prompt: the
    /// network-to-device pump sees EOF at once, which ends the bridge, which
    /// releases the accept loop to notice the stop flag. Without it, stopping
    /// would block until the client happened to disconnect on its own.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);

        {
            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(stream) = active.as_ref() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }

        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            log::warn!("a port supervisor panicked; its device may stay busy until exit");
        }
    }
}

/// Open the device, bind its listener, and start supervising.
pub fn start(
    entry: &Arc<PortEntry>,
    cfg: &PortConfig,
    opener: &DeviceOpener,
    allow: Arc<Allowlist>,
) -> Result<Control> {
    let port = opener(&cfg.device, &cfg.serial)
        .with_context(|| format!("failed to open {}", cfg.device))?;

    let listener = bind_listener(entry.bind_addr())?;
    let bound = listener
        .local_addr()
        .context("failed to read the listener's address")?;
    listener
        .set_nonblocking(true)
        .context("failed to make the listener non-blocking")?;

    // A dedicated write handle, shared rather than owned by the session, so the
    // dashboard can talk to the device between clients.
    let writer = port
        .try_clone()
        .context("failed to clone the port for writing")?;
    let inject = share(Box::new(writer) as Box<dyn Write + Send>);

    let stop = Arc::new(AtomicBool::new(false));
    let active: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));

    let options = Options {
        protocol: cfg.protocol,
        virtual_line: false,
        settings: cfg.serial.clone(),
    };

    log::info!(
        "port {} serving {} on {bound}",
        entry.id,
        cfg.device.as_str()
    );
    if !bound.ip().is_loopback() {
        if allow.is_empty() {
            log::warn!(
                "{bound} is reachable from the network and the connection is unauthenticated \
                 and unencrypted — anyone who can reach this port controls {}",
                cfg.device
            );
        } else {
            log::info!("{bound} accepts connections from {} only", allow.describe());
        }
    }

    let handle = thread::Builder::new()
        .name(format!("port-{}", entry.id))
        .spawn({
            let entry = Arc::clone(entry);
            let stop = Arc::clone(&stop);
            let active = Arc::clone(&active);
            let inject = Arc::clone(&inject);
            let duties = Duties {
                entry,
                inject,
                options,
                stop,
                active,
                allow,
            };
            move || supervise(port, listener, duties)
        })
        .context("failed to spawn the port supervisor")?;

    Ok(Control {
        stop,
        handle: Some(handle),
        active,
        inject,
        bound,
    })
}

/// Everything the supervisor thread carries for the life of one running port.
struct Duties {
    entry: Arc<PortEntry>,
    inject: SharedWriter,
    options: Options,
    stop: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
    allow: Arc<Allowlist>,
}

fn supervise(port: Box<dyn SerialPort>, listener: TcpListener, duties: Duties) {
    let Duties {
        entry,
        inject,
        options,
        stop,
        active,
        allow,
    } = duties;

    // Used only between sessions, to feed the browser's monitor while no TCP
    // client is attached. Safe precisely because it is only ever read from in
    // the branch below: while a session is running, this loop is parked inside
    // `session_with`, and a second reader would steal that session's bytes.
    let mut idle_reader = port.try_clone().ok();

    while !stop.load(Ordering::Relaxed) {
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            // Nothing waiting: the normal case, and the moment a stop gets
            // noticed.
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                idle_tick(&mut idle_reader, &entry);
                continue;
            }
            Err(e) => {
                log::warn!("{}: accept failed: {e}", entry.id);
                entry.state.set_error(format!("accept failed: {e}"));
                thread::sleep(IO_TIMEOUT);
                continue;
            }
        };

        // These ports speak raw bytes or RFC 2217 and have no way to ask who is
        // calling, so where the connection came from is the only thing there is
        // to go on. Refuse before the device is touched at all.
        if !allow.permits(peer.ip()) {
            log::warn!(
                "{}: refused {peer}, which is outside {}",
                entry.id,
                allow.describe()
            );
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }

        // A socket accepted from a non-blocking listener can inherit that mode,
        // which would turn every pump read into a busy spin. `tcp_halves` sets
        // the read timeout it wants; it cannot undo non-blocking.
        if let Err(e) = stream.set_nonblocking(false) {
            log::warn!(
                "{}: could not put the client socket in blocking mode: {e}",
                entry.id
            );
            continue;
        }

        // Whatever the device said while nobody was listening is not this
        // client's data, and would arrive as a corrupt partial frame.
        if let Err(e) = port.clear(ClearBuffer::All) {
            log::debug!("{}: could not clear serial buffers: {e}", entry.id);
        }

        match stream.try_clone() {
            Ok(clone) => *active.lock().unwrap_or_else(|e| e.into_inner()) = Some(clone),
            Err(e) => log::debug!("{}: could not clone the client socket: {e}", entry.id),
        }
        entry.state.set_client(Some(peer.to_string()));
        log::info!("{}: client connected from {peer}", entry.id);

        match session_halves(port.as_ref(), &entry, &inject) {
            Ok(serial) => match server::session_with(port.as_ref(), serial, stream, &options) {
                Ok(stats) => log::info!(
                    "{}: client {peer} disconnected ({} bytes from device, {} bytes to device)",
                    entry.id,
                    stats.a_to_b,
                    stats.b_to_a
                ),
                Err(e) => {
                    log::warn!("{}: session with {peer} failed: {e:#}", entry.id);
                    entry.state.set_error(format!("{e:#}"));
                }
            },
            Err(e) => {
                log::warn!("{}: could not start a session for {peer}: {e:#}", entry.id);
                entry.state.set_error(format!("{e:#}"));
            }
        }

        *active.lock().unwrap_or_else(|e| e.into_inner()) = None;
        entry.state.set_client(None);
    }

    log::info!("{}: stopped", entry.id);
}

/// What to do with a spare moment between clients.
///
/// A serial line only gets read when something is bridged to it, which would
/// mean the dashboard's monitor stayed blank until a TCP client happened to
/// connect — useless for the main thing people want it for, watching a device
/// that is talking. So when somebody is watching, drain the line and publish
/// what it said; the bytes are still discarded rather than delivered, exactly as
/// they are today.
///
/// With nobody watching, do nothing but wait: reading here would throw away data
/// that the next client should have been the first to see.
fn idle_tick(reader: &mut Option<Box<dyn SerialPort>>, entry: &Arc<PortEntry>) {
    if entry.tap.watchers() == 0 {
        thread::sleep(IO_TIMEOUT);
        return;
    }
    let Some(reader) = reader.as_mut() else {
        thread::sleep(IO_TIMEOUT);
        return;
    };

    let mut buf = [0u8; 4096];
    match reader.read(&mut buf) {
        Ok(0) => thread::sleep(IO_TIMEOUT),
        Ok(n) => entry.tap.publish(Dir::Rx, &buf[..n]),
        // A timeout is the idle case, and the read has already done the waiting.
        Err(_) => {}
    }
}

/// The device side of one session, wrapped so every byte is counted and mirrored
/// to anyone watching in a browser.
fn session_halves(
    port: &dyn SerialPort,
    entry: &Arc<PortEntry>,
    inject: &SharedWriter,
) -> Result<Halves> {
    let reader = port
        .try_clone()
        .context("failed to clone the port for reading")?;

    Ok((
        Box::new(TapReader::new(
            Box::new(reader),
            Arc::clone(&entry.tap),
            Dir::Rx,
        )),
        Box::new(TapSink::new(Arc::clone(inject), Arc::clone(&entry.tap))),
    ))
}
