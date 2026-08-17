//! The set of ports the dashboard knows about, and their lifecycles.
//!
//! Two kinds of thread touch this: HTTP handlers, which read state constantly
//! and occasionally ask for a change, and one supervisor thread per running
//! port. The rule that keeps them out of each other's way is that **the registry
//! lock is never held across blocking I/O**. Handlers lock only long enough to
//! clone an `Arc<PortEntry>` out, then work on that; supervisors never take the
//! registry lock at all, only the small mutexes on their own entry.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serialport::SerialPort;

use crate::cli::{ProtocolArg, SerialArgs};
use crate::dashboard::config::{Config, PortConfig, slug};
use crate::dashboard::supervisor::{self, Control};
use crate::dashboard::tap::{Tap, TapSink};
use crate::serial;

pub type PortId = String;

/// How a device gets opened. Indirected so tests can supply an in-memory port
/// instead of demanding real hardware.
pub type DeviceOpener = Arc<dyn Fn(&str, &SerialArgs) -> Result<Box<dyn SerialPort>> + Send + Sync>;

pub fn real_devices() -> DeviceOpener {
    Arc::new(serial::open)
}

pub struct Registry {
    entries: Mutex<Vec<Arc<PortEntry>>>,
    config_path: PathBuf,
    base_port: u16,
    token: String,
    opener: DeviceOpener,
}

impl Registry {
    pub fn new(config: &Config, config_path: PathBuf, opener: DeviceOpener) -> Arc<Self> {
        let entries = config
            .ports
            .iter()
            .map(|p| Arc::new(PortEntry::new(p.clone())))
            .collect();

        Arc::new(Self {
            entries: Mutex::new(entries),
            config_path,
            base_port: config.base_port,
            token: config.token.clone(),
            opener,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    /// A snapshot of the entries. Cloning the `Arc`s frees the lock immediately.
    pub fn entries(&self) -> Vec<Arc<PortEntry>> {
        self.lock().clone()
    }

    pub fn entry(&self, id: &str) -> Option<Arc<PortEntry>> {
        self.lock().iter().find(|e| e.id == id).map(Arc::clone)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<PortEntry>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pair a device: register it and give it a TCP port of its own.
    pub fn add(
        &self,
        device: &str,
        label: &str,
        protocol: ProtocolArg,
        settings: SerialArgs,
        tcp_port: Option<u16>,
        expose: bool,
    ) -> Result<Arc<PortEntry>> {
        let mut entries = self.lock();

        // One entry per device: serial ports are exclusive, and two entries for
        // one device would just mean the second never starts.
        if entries.iter().any(|e| e.device == device) {
            bail!("{device} is already paired");
        }

        let taken: Vec<u16> = entries.iter().map(|e| e.tcp_port()).collect();
        let tcp_port = match tcp_port {
            Some(requested) => {
                if taken.contains(&requested) {
                    bail!("TCP port {requested} is already assigned to another serial port");
                }
                requested
            }
            None => allocate_port(self.base_port, &taken)?,
        };

        let id = unique_id(&slug(device), &entries);
        let entry = Arc::new(PortEntry::new(PortConfig {
            id: id.clone(),
            device: device.to_owned(),
            label: label.to_owned(),
            tcp_port,
            protocol,
            serial: settings,
            expose,
            autostart: false,
        }));
        entries.push(Arc::clone(&entry));
        drop(entries);

        self.persist()?;
        Ok(entry)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).ok_or_else(|| unknown(id))?;
        entry.stop();

        self.lock().retain(|e| e.id != id);
        self.persist()?;
        Ok(())
    }

    /// Apply a change to a port's configuration.
    ///
    /// Serial settings are cached per handle, and the codebase never mutates
    /// them after splitting a port into halves (see `endpoint::serial_halves`),
    /// so a running port has to be stopped and reopened for the new settings to
    /// mean anything. That drops whatever client was connected — the UI warns
    /// about it before saving.
    pub fn update(&self, id: &str, patch: PortPatch) -> Result<Arc<PortEntry>> {
        let entry = self.entry(id).ok_or_else(|| unknown(id))?;

        if let Some(requested) = patch.tcp_port {
            let clash = self
                .lock()
                .iter()
                .any(|e| e.id != id && e.tcp_port() == requested);
            if clash {
                bail!("TCP port {requested} is already assigned to another serial port");
            }
        }

        let was_running = entry.is_running();
        if was_running {
            entry.stop();
        }

        {
            let mut cfg = entry.cfg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = patch.label {
                cfg.label = v;
            }
            if let Some(v) = patch.tcp_port {
                cfg.tcp_port = v;
            }
            if let Some(v) = patch.protocol {
                cfg.protocol = v;
            }
            if let Some(v) = patch.serial {
                cfg.serial = v;
            }
            if let Some(v) = patch.expose {
                cfg.expose = v;
            }
            if let Some(v) = patch.autostart {
                cfg.autostart = v;
            }
        }

        self.persist()?;

        if was_running {
            self.start(id)?;
        }
        Ok(entry)
    }

    pub fn start(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).ok_or_else(|| unknown(id))?;
        if entry.is_running() {
            return Ok(());
        }

        let cfg = entry.config();
        let result = supervisor::start(&entry, &cfg, &self.opener);

        match result {
            Ok(control) => {
                *entry.control.lock().unwrap_or_else(|e| e.into_inner()) = Some(control);
                entry.state.running.store(true, Ordering::Relaxed);
                entry.state.set_started_now();
                entry.state.clear_error();
                Ok(())
            }
            Err(e) => {
                entry.state.set_error(format!("{e:#}"));
                Err(e)
            }
        }
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).ok_or_else(|| unknown(id))?;
        entry.stop();
        Ok(())
    }

    /// Bring up everything marked `autostart`. Failures are recorded on the
    /// entry rather than aborting the run: one unplugged device should not stop
    /// the dashboard from coming up.
    pub fn autostart_all(&self) {
        for entry in self.entries() {
            if !entry.config().autostart {
                continue;
            }
            if let Err(e) = self.start(&entry.id) {
                log::warn!("could not autostart {}: {e:#}", entry.device);
            }
        }
    }

    pub fn shutdown(&self) {
        for entry in self.entries() {
            entry.stop();
        }
    }

    /// Write the current set of ports back to disk.
    pub fn persist(&self) -> Result<()> {
        let ports = self.lock().iter().map(|e| e.config()).collect();
        let config = Config {
            version: crate::dashboard::config::CURRENT_VERSION,
            token: self.token.clone(),
            base_port: self.base_port,
            ports,
        };
        config.save(&self.config_path)
    }
}

/// Fields a `PATCH` may change. `None` means "leave it alone".
#[derive(Debug, Default)]
pub struct PortPatch {
    pub label: Option<String>,
    pub tcp_port: Option<u16>,
    pub protocol: Option<ProtocolArg>,
    pub serial: Option<SerialArgs>,
    pub expose: Option<bool>,
    pub autostart: Option<bool>,
}

pub struct PortEntry {
    pub id: PortId,
    pub device: String,
    pub cfg: Mutex<PortConfig>,
    pub state: PortState,
    pub tap: Arc<Tap>,
    pub control: Mutex<Option<Control>>,
}

impl PortEntry {
    fn new(cfg: PortConfig) -> Self {
        Self {
            id: cfg.id.clone(),
            device: cfg.device.clone(),
            cfg: Mutex::new(cfg),
            state: PortState::default(),
            tap: Tap::new(),
            control: Mutex::new(None),
        }
    }

    pub fn config(&self) -> PortConfig {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn tcp_port(&self) -> u16 {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).tcp_port
    }

    pub fn is_running(&self) -> bool {
        self.state.running.load(Ordering::Relaxed)
    }

    /// The address the listener actually got, once running. Differs from the
    /// configured one when port 0 was asked for, which is how tests avoid
    /// fighting over fixed ports.
    pub fn bound(&self) -> Option<SocketAddr> {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|control| control.bound)
    }

    /// Tear the port down and release the device.
    ///
    /// Taking the `Control` out of its mutex first means the lock is not held
    /// while joining, and dropping it at the end of this function is what closes
    /// the last handle to the hardware.
    pub fn stop(&self) {
        let control = self
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        if let Some(control) = control {
            control.shutdown();
        }

        self.state.running.store(false, Ordering::Relaxed);
        self.state.set_client(None);
        self.state.clear_started();
    }

    /// Write bytes straight to the device.
    ///
    /// Goes through the same shared handle the bridge uses, so this can never
    /// interleave with a connected client's traffic, and works with no client
    /// connected at all — the handle belongs to the port, not to the session.
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let inject = {
            let control = self.control.lock().unwrap_or_else(|e| e.into_inner());
            match control.as_ref() {
                Some(control) => control.inject(),
                None => bail!("{} is not running", self.device),
            }
        };

        let mut sink = TapSink::new(inject, Arc::clone(&self.tap));
        sink.write_all(bytes)
            .and_then(|()| sink.flush())
            .with_context(|| format!("failed to write to {}", self.device))
    }

    /// The address this port's listener should bind to.
    pub fn bind_addr(&self) -> SocketAddr {
        let cfg = self.config();
        let ip = if cfg.expose {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        };
        SocketAddr::from((ip, cfg.tcp_port))
    }
}

/// Everything about a port that changes while it runs.
#[derive(Default)]
pub struct PortState {
    pub running: AtomicBool,
    started_at: Mutex<Option<Instant>>,
    client: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
}

impl PortState {
    pub fn set_started_now(&self) {
        *self.started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    pub fn clear_started(&self) {
        *self.started_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn uptime_secs(&self) -> Option<u64> {
        self.started_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed().as_secs())
    }

    pub fn set_client(&self, peer: Option<String>) {
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = peer;
    }

    pub fn client(&self) -> Option<String> {
        self.client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_error(&self, message: String) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }

    pub fn clear_error(&self) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The lowest free port at or above `base`, skipping ones already handed out.
///
/// The trial bind only weeds out ports something else on the machine is already
/// using; it races by definition, so the real bind at start time is still what
/// decides. Better to catch the common collision here than to hand out a port
/// that obviously cannot work.
fn allocate_port(base: u16, taken: &[u16]) -> Result<u16> {
    for candidate in base..=u16::MAX {
        if taken.contains(&candidate) {
            continue;
        }
        if TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)).is_ok() {
            return Ok(candidate);
        }
    }
    bail!("no free TCP port at or above {base}")
}

fn unique_id(base: &str, entries: &[Arc<PortEntry>]) -> String {
    if !entries.iter().any(|e| e.id == base) {
        return base.to_owned();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !entries.iter().any(|e| e.id == candidate) {
            return candidate;
        }
    }
    unreachable!("the loop above only ends by returning")
}

fn unknown(id: &str) -> anyhow::Error {
    anyhow!("no port with id {id}")
}

/// Bind a listener for a port, turning the OS error into something a user can
/// act on.
pub(crate) fn bind_listener(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).with_context(|| {
        format!("failed to listen on {addr} (is another program already using that port?)")
    })
}
