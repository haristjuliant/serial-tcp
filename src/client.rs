//! `serial-tcp connect` — attach to a remote `serve` and expose it locally.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::bridge::bridge;
use crate::cli::{ConnectArgs, ProtocolArg};
use crate::endpoint::{Halves, IO_TIMEOUT, serial_halves, stdio_halves, tcp_halves};
use crate::rfc2217::codec::{Decoder, EscapingWriter, TelnetReader, share};
use crate::rfc2217::comport::ClientHandler;
use crate::serial;

pub fn run(args: ConnectArgs) -> Result<()> {
    let stream = TcpStream::connect(&args.to)
        .with_context(|| format!("failed to connect to {}", args.to))?;
    log::info!("connected to {}", args.to);

    let remote = match args.protocol {
        ProtocolArg::Raw => tcp_halves(stream)?,
        ProtocolArg::Rfc2217 => rfc2217_halves(stream, &args)?,
    };

    // Held for the lifetime of the session so the pseudo-terminal pair, if we
    // made one, does not collapse the moment the attached program disconnects.
    let mut _pty_keepalive = None;

    let (local, label) = if args.stdio {
        log::info!("piping to stdin/stdout");
        (stdio_halves(), "stdio")
    } else if args.pty {
        let pty = serial::open_pty()?;
        println!("virtual serial port ready at {}", pty.path);
        println!("point your application at that path");
        let halves = serial_halves(pty.master.as_ref())?;
        _pty_keepalive = Some(pty);
        (halves, "pty")
    } else {
        // clap's ArgGroup guarantees --port is present if we reach here.
        let path = args.port.as_deref().expect("clap enforces one target");
        let port = serial::open(path, &args.serial)?;
        log::info!("bridging to local port {path}");
        (serial_halves(port.as_ref())?, "serial")
    };

    let stats = bridge(remote, local, "tcp", label)?;
    log::info!(
        "session ended ({} bytes from remote, {} bytes to remote)",
        stats.a_to_b,
        stats.b_to_a
    );
    Ok(())
}

/// Wrap the socket in Telnet framing and ask the far end for our line settings.
fn rfc2217_halves(stream: TcpStream, args: &ConnectArgs) -> Result<Halves> {
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("failed to set TCP read timeout")?;

    let socket = share(Box::new(
        stream.try_clone().context("failed to clone TCP stream")?,
    ));
    let mut handler = ClientHandler::new(Arc::clone(&socket), args.serial.clone());

    // Offer the options first; the settings themselves go out once the server
    // confirms it speaks COM-PORT-OPTION, via `Handler::option_agreed`.
    let mut decoder = Decoder::new();
    decoder
        .initiate(&mut handler)
        .context("failed to start Telnet negotiation")?;

    Ok((
        Box::new(TelnetReader::new(stream, decoder, handler)) as Box<dyn Read + Send>,
        Box::new(EscapingWriter::new(socket)) as Box<dyn Write + Send>,
    ))
}
