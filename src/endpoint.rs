//! Turning the things we can bridge (serial ports, sockets, stdio) into a
//! uniform pair of owned halves that can each move to their own thread.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result};
use serialport::SerialPort;

/// Read timeout applied to every endpoint we can configure.
///
/// A thread parked in a blocking `read` cannot notice that the *other*
/// direction has died, so nothing would ever tear the session down. Waking up
/// this often lets each pump re-check the shutdown flag without meaningfully
/// costing anything.
pub const IO_TIMEOUT: Duration = Duration::from_millis(50);

/// The read half and write half of one endpoint.
pub type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>);

/// Split a serial port into independent halves.
///
/// `try_clone` is explicitly supported for reading and writing simultaneously.
/// Its documented hazard is that settings are cached per handle, so changing
/// them through two handles misbehaves — we therefore never mutate settings
/// after this point.
pub fn serial_halves(port: &dyn SerialPort) -> Result<Halves> {
    let reader = port
        .try_clone()
        .context("failed to clone serial port for reading")?;
    let writer = port
        .try_clone()
        .context("failed to clone serial port for writing")?;
    Ok((Box::new(reader), Box::new(writer)))
}

/// Split a TCP stream into independent halves.
pub fn tcp_halves(stream: TcpStream) -> Result<Halves> {
    // Serial traffic is small and latency-sensitive; Nagle would coalesce bytes
    // and smear the inter-frame gaps that protocols like Modbus RTU rely on.
    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY")?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("failed to set TCP read timeout")?;

    let writer = stream.try_clone().context("failed to clone TCP stream")?;
    Ok((Box::new(stream), Box::new(writer)))
}

/// Split this process's stdin/stdout.
///
/// Note that stdin has no read timeout, so its pump cannot observe the shutdown
/// flag; [`crate::bridge::bridge`] returns as soon as either direction ends and
/// leaves that thread parked until the process exits.
pub fn stdio_halves() -> Halves {
    (Box::new(std::io::stdin()), Box::new(std::io::stdout()))
}
