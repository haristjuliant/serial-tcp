//! Everything that talks to the `serialport` crate lives here.
//!
//! Keeping it in one file is deliberate: the crate is currently looking for
//! maintainers, so if it ever has to be swapped out this is the only module
//! that changes.

use anyhow::{Context, Result};
use serialport::SerialPort;

use crate::cli::SerialArgs;
use crate::endpoint::IO_TIMEOUT;

/// Open a serial port with the given line settings.
pub fn open(path: &str, settings: &SerialArgs) -> Result<Box<dyn SerialPort>> {
    serialport::new(path, settings.baud)
        .data_bits(settings.data_bits.into())
        .parity(settings.parity.into())
        .stop_bits(settings.stop_bits.into())
        .flow_control(settings.flow_control.into())
        .timeout(IO_TIMEOUT)
        .open()
        .with_context(|| {
            format!(
                "failed to open serial port {path} \
                 (is it already in use? run `serial-tcp list` to see what is available)"
            )
        })
}

/// A pseudo-terminal pair standing in for a real serial device.
pub struct Pty {
    /// The end this process drives.
    pub master: Box<dyn SerialPort>,
    /// Filesystem path of the end other programs should open.
    pub path: String,
    /// Held open for as long as the pair is in use, never read from.
    ///
    /// On Linux a master whose slave has been closed reports EIO rather than
    /// simply going quiet, so dropping this would break the pair the moment the
    /// attached program disconnects.
    pub keepalive: Box<dyn SerialPort>,
}

#[cfg(unix)]
pub fn open_pty() -> Result<Pty> {
    use serialport::TTYPort;

    let (mut master, slave) = TTYPort::pair().context("failed to create a pseudo-terminal pair")?;
    master
        .set_timeout(IO_TIMEOUT)
        .context("failed to set pseudo-terminal timeout")?;

    let path = slave
        .name()
        .context("pseudo-terminal has no filesystem path")?;

    Ok(Pty {
        master: Box::new(master),
        path,
        keepalive: Box::new(slave),
    })
}

#[cfg(not(unix))]
pub fn open_pty() -> Result<Pty> {
    anyhow::bail!(
        "Windows has no pseudo-terminals, so a virtual port cannot be created from user space.\n\
         Install com0com (https://com0com.com/), create a pair such as COM10<->COM11, then use \
         `--port COM10` here and point your application at COM11."
    )
}
