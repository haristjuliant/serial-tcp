//! `serial-tcp serve` — share a local serial port over TCP.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::{Context, Result};
use serialport::{ClearBuffer, SerialPort};

use crate::bridge::{Stats, bridge};
use crate::cli::{ProtocolArg, SerialArgs, ServeArgs};
use crate::endpoint::{Halves, IO_TIMEOUT, serial_halves, tcp_halves};
use crate::rfc2217::codec::{Decoder, EscapingWriter, TelnetReader, share};
use crate::rfc2217::comport::{ServerHandler, spawn_modem_notifier};
use crate::serial;

pub fn run(args: ServeArgs) -> Result<()> {
    let device = open_device(&args)?;

    let listener = TcpListener::bind(&args.bind)
        .with_context(|| format!("failed to listen on {}", args.bind))?;
    let addr = listener
        .local_addr()
        .context("failed to read local address")?;

    log::info!("serving {} on {addr}", device.label);
    if !addr.ip().is_loopback() {
        log::warn!(
            "{addr} is reachable from the network and the connection is unauthenticated \
             and unencrypted — anyone who can reach this port controls the device"
        );
    }

    let options = Options {
        protocol: args.protocol,
        virtual_line: device.virtual_line,
        settings: args.serial.clone(),
    };
    serve_on(&listener, device.port.as_ref(), &options, None)
}

/// How to run each client session.
#[derive(Clone)]
pub struct Options {
    pub protocol: ProtocolArg,
    /// True when the "device" is a pseudo-terminal, which has no real serial
    /// line and therefore no line settings that can meaningfully be applied.
    pub virtual_line: bool,
    /// Line settings the server was started with, used as the starting point
    /// reported to RFC 2217 clients.
    pub settings: SerialArgs,
}

impl Options {
    /// A plain byte pipe over a real device.
    pub fn raw() -> Self {
        Self {
            protocol: ProtocolArg::Raw,
            virtual_line: false,
            settings: SerialArgs::default(),
        }
    }
}

/// Accept and bridge clients on an already-bound listener.
///
/// `max_sessions` bounds how many clients to serve before returning; `None`
/// serves forever. Only one client is bridged at a time — two writers on one
/// serial line would interleave into garbage.
pub fn serve_on(
    listener: &TcpListener,
    port: &dyn SerialPort,
    options: &Options,
    max_sessions: Option<usize>,
) -> Result<()> {
    for (index, stream) in listener.incoming().enumerate() {
        let stream = stream.context("failed to accept a connection")?;
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "unknown".to_owned(), |a| a.to_string());
        log::info!("client connected from {peer}");

        // Whatever the device said while nobody was listening is not this
        // client's data, and would arrive as a corrupt partial frame.
        if let Err(e) = port.clear(ClearBuffer::All) {
            log::debug!("could not clear serial buffers: {e}");
        }

        let stats = session(port, stream, options)?;
        log::info!(
            "client {peer} disconnected ({} bytes from device, {} bytes to device)",
            stats.a_to_b,
            stats.b_to_a
        );

        if max_sessions.is_some_and(|max| index + 1 >= max) {
            break;
        }
    }

    Ok(())
}

fn session(port: &dyn SerialPort, stream: TcpStream, options: &Options) -> Result<Stats> {
    match options.protocol {
        ProtocolArg::Raw => Ok(bridge(
            serial_halves(port)?,
            tcp_halves(stream)?,
            "serial",
            "tcp",
        )?),
        ProtocolArg::Rfc2217 => rfc2217_session(port, stream, options),
    }
}

/// One RFC 2217 session.
///
/// The Telnet framing is confined to the two adapters wrapped around the socket
/// here, so the bridge underneath still sees nothing but a reader and a writer.
fn rfc2217_session(port: &dyn SerialPort, stream: TcpStream, options: &Options) -> Result<Stats> {
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("failed to set TCP read timeout")?;

    let socket = share(Box::new(
        stream.try_clone().context("failed to clone TCP stream")?,
    ));

    // Notifications are on by default: a client that never sets a mask should
    // still learn when CTS or CD moves. An idle line produces no traffic.
    let modem_mask = Arc::new(AtomicU8::new(0xFF));
    let stop_notifier = Arc::new(AtomicBool::new(false));
    let notifier = spawn_modem_notifier(
        port.try_clone()
            .context("failed to clone port for notifier")?,
        Arc::clone(&socket),
        Arc::clone(&modem_mask),
        Arc::clone(&stop_notifier),
    );

    let handler = ServerHandler::new(
        port.try_clone()
            .context("failed to clone port for control")?,
        Arc::clone(&socket),
        modem_mask,
        options.virtual_line,
        &options.settings,
    );

    let tcp: Halves = (
        Box::new(TelnetReader::new(stream, Decoder::new(), handler)) as Box<dyn Read + Send>,
        Box::new(EscapingWriter::new(socket)) as Box<dyn Write + Send>,
    );

    let stats = bridge(serial_halves(port)?, tcp, "serial", "tcp");

    stop_notifier.store(true, Ordering::Relaxed);
    let _ = notifier.join();

    stats
}

struct Device {
    port: Box<dyn SerialPort>,
    label: String,
    virtual_line: bool,
    /// Keeps a `--fake` pseudo-terminal pair alive. See [`serial::Pty`].
    _keepalive: Option<Box<dyn SerialPort>>,
}

fn open_device(args: &ServeArgs) -> Result<Device> {
    if args.fake {
        let pty = serial::open_pty()?;
        println!("fake device ready at {}", pty.path);
        println!("attach a program to that path to act as the serial device");
        return Ok(Device {
            port: pty.master,
            label: format!("pseudo-terminal {}", pty.path),
            virtual_line: true,
            _keepalive: Some(pty.keepalive),
        });
    }

    // clap's ArgGroup guarantees one of --port / --fake is present.
    let path = args
        .port
        .as_deref()
        .expect("clap enforces --port or --fake");
    Ok(Device {
        port: serial::open(path, &args.serial)?,
        label: path.to_owned(),
        virtual_line: false,
        _keepalive: None,
    })
}
