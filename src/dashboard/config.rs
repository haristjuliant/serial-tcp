//! What the dashboard remembers between runs.
//!
//! The file is written atomically — temp file, then rename — because the
//! alternative is a truncated config after a power cut, which would lose every
//! paired port. A file we cannot parse is moved aside rather than deleted, and
//! the dashboard starts empty instead of refusing to boot: being locked out of
//! the control panel by its own config file would be the worse failure.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::cli::{ProtocolArg, SerialArgs};

/// Bumped only when the shape changes incompatibly. A file from the future is
/// treated the same as a corrupt one.
pub const CURRENT_VERSION: u32 = 1;

/// Length of a generated token in bytes, before hex encoding.
const TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub token: String,
    pub base_port: u16,
    #[serde(default)]
    pub ports: Vec<PortConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortConfig {
    /// Stable handle used in URLs. Derived from the device name.
    pub id: String,
    /// The device as the OS names it: `COM8`, `/dev/cu.usbserial-1410`.
    pub device: String,
    #[serde(default)]
    pub label: String,
    pub tcp_port: u16,
    #[serde(default = "raw_protocol")]
    pub protocol: ProtocolArg,
    /// Line settings, flattened so the file reads as one object per port
    /// rather than nesting `{"serial": {...}}` a level deeper.
    #[serde(flatten)]
    pub serial: SerialArgs,
    /// Whether this port's TCP listener accepts connections from the network.
    /// Off by default, mirroring `serve`'s deliberate loopback default.
    #[serde(default)]
    pub expose: bool,
    #[serde(default)]
    pub autostart: bool,
}

fn raw_protocol() -> ProtocolArg {
    ProtocolArg::Raw
}

impl Config {
    pub fn new(base_port: u16, token: String) -> Self {
        Self {
            version: CURRENT_VERSION,
            token,
            base_port,
            ports: Vec::new(),
        }
    }

    /// Read the config, or start a fresh one if there is nothing usable there.
    ///
    /// `token_override` wins over whatever is on disk and is not written back,
    /// so `--token` / `SERIAL_TCP_TOKEN` can be used without rewriting the file.
    pub fn load_or_create(
        path: &Path,
        base_port: u16,
        token_override: Option<String>,
    ) -> Result<Self> {
        let mut config = match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(config) if config.version <= CURRENT_VERSION => config,
                Ok(config) => {
                    let backup = quarantine(path, &text)?;
                    log::warn!(
                        "{} is version {} but this build understands at most {CURRENT_VERSION}; \
                         moved it to {} and starting fresh",
                        path.display(),
                        config.version,
                        backup.display()
                    );
                    Self::new(base_port, generate_token()?)
                }
                Err(e) => {
                    let backup = quarantine(path, &text)?;
                    log::warn!(
                        "could not parse {} ({e}); moved it to {} and starting fresh",
                        path.display(),
                        backup.display()
                    );
                    Self::new(base_port, generate_token()?)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config at {}, starting a new one", path.display());
                Self::new(base_port, generate_token()?)
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        if let Some(token) = token_override {
            config.token = token;
        }
        if config.token.is_empty() {
            config.token = generate_token()?;
        }

        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialise the configuration")?;

        // Write beside the target so the rename below stays on one filesystem.
        let tmp = temp_path(path);
        {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("failed to create {}", tmp.display()))?;
            file.write_all(json.as_bytes())
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.write_all(b"\n").ok();
            // Without this the rename can land before the contents do.
            file.sync_all()
                .with_context(|| format!("failed to flush {}", tmp.display()))?;
        }

        restrict_permissions(&tmp);

        fs::rename(&tmp, path)
            .with_context(|| format!("failed to move {} into place", tmp.display()))?;
        Ok(())
    }

    pub fn port(&self, id: &str) -> Option<&PortConfig> {
        self.ports.iter().find(|p| p.id == id)
    }
}

/// 32 random bytes, hex encoded. Guessing one is not a realistic attack, which
/// is the whole point — this is the only thing standing between the network and
/// the hardware.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow!("failed to read random bytes for the access token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Turn a device name into something safe to put in a URL: `COM8` -> `com8`,
/// `/dev/cu.usbserial-1410` -> `dev-cu-usbserial-1410`.
pub fn slug(device: &str) -> String {
    let mut out = String::with_capacity(device.len());
    let mut pending_dash = false;
    for c in device.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "port".to_owned()
    } else {
        out
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Move an unusable config aside, keeping its contents for the user to inspect.
fn quarantine(path: &Path, contents: &str) -> Result<PathBuf> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    let backup = path.with_file_name(name);
    fs::write(&backup, contents)
        .with_context(|| format!("failed to save the old config to {}", backup.display()))?;
    Ok(backup)
}

/// The token lives in this file, so keep it to the owner where the OS has a
/// concept for that. Best effort: a failure here should not stop the dashboard.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        log::debug!("could not restrict permissions on {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
