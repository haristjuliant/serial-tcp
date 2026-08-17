//! Everything that talks to the `serialport` crate lives here.
//!
//! Keeping it in one file is deliberate: the crate is currently looking for
//! maintainers, so if it ever has to be swapped out this is the only module
//! that changes.

use std::path::PathBuf;

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

/// Directory where stable pseudo-terminal links live.
fn link_dir() -> PathBuf {
    std::env::temp_dir().join("serial-tcp")
}

/// Path of the stable link for a given remote address. The OS hands out a
/// different pty name (`/dev/ttys001`, `/dev/ttys003`, ...) on every
/// reconnect, which makes it impossible to point a test setup at a fixed
/// path; this gives the same `--to` address the same local path every time.
pub fn link_path(to: &str) -> PathBuf {
    link_dir().join(to.replace('/', "-"))
}

/// Point `link` at a pty's device path, replacing whatever was there before.
///
/// Symlinks into a temporary name first and renames over `link`, so a program
/// that happens to open the path mid-refresh never sees it briefly missing.
#[cfg(unix)]
fn refresh_link(link: &std::path::Path, target: &str) -> Result<()> {
    let dir = link.parent().expect("link always has a parent");
    std::fs::create_dir_all(dir).context("failed to create the pseudo-terminal link directory")?;

    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    std::os::unix::fs::symlink(target, &tmp)
        .with_context(|| format!("failed to create a symlink to {target}"))?;
    std::fs::rename(&tmp, link)
        .with_context(|| format!("failed to move the symlink into place at {}", link.display()))
}

/// Create or refresh the stable link for `to` so it points at `pty`.
#[cfg(unix)]
pub fn link_pty(pty: &Pty, to: &str) -> Result<PathBuf> {
    let link = link_path(to);
    refresh_link(&link, &pty.path)?;
    Ok(link)
}

#[cfg(not(unix))]
pub fn link_pty(_pty: &Pty, _to: &str) -> Result<PathBuf> {
    anyhow::bail!("pseudo-terminal links are Unix only")
}
