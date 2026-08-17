//! The JSON the dashboard page talks to.
//!
//! `serialport`'s own types are foreign, so they cannot carry serde impls of
//! ours; the DTOs here are the translation layer. Line settings are flattened
//! into their parent object so a port reads as one flat thing in both the API
//! and the config file, rather than nesting a `serial` object a level down.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{DataBitsArg, FlowControlArg, ParityArg, ProtocolArg, SerialArgs, StopBitsArg};
use crate::dashboard::registry::{PortEntry, PortPatch, Registry};
use crate::list;

/// A failure with the status code it should be reported as.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(404, what)
    }

    pub fn bad_request(what: impl Into<String>) -> Self {
        Self::new(400, what)
    }
}

/// Registry failures are nearly all things the user asked for that cannot be
/// done — a device already paired, a port already in use, hardware that will not
/// open. Those are 400s, not 500s.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(400, format!("{e:#}"))
    }
}

pub type ApiResult = Result<Value, ApiError>;

// ---------------------------------------------------------------- responses

#[derive(Serialize)]
struct DeviceDto {
    device: String,
    description: Vec<String>,
    paired: bool,
}

#[derive(Serialize)]
struct PortDto {
    id: String,
    device: String,
    label: String,
    tcp_port: u16,
    protocol: ProtocolArg,
    #[serde(flatten)]
    serial: SerialArgs,
    expose: bool,
    autostart: bool,
    running: bool,
    client: Option<String>,
    rx_bytes: u64,
    tx_bytes: u64,
    uptime_secs: Option<u64>,
    last_error: Option<String>,
    watchers: usize,
}

fn port_dto(entry: &Arc<PortEntry>) -> PortDto {
    let cfg = entry.config();
    PortDto {
        id: cfg.id,
        device: cfg.device,
        label: cfg.label,
        tcp_port: cfg.tcp_port,
        protocol: cfg.protocol,
        serial: cfg.serial,
        expose: cfg.expose,
        autostart: cfg.autostart,
        running: entry.is_running(),
        client: entry.state.client(),
        rx_bytes: entry.tap.rx_bytes(),
        tx_bytes: entry.tap.tx_bytes(),
        uptime_secs: entry.state.uptime_secs(),
        last_error: entry.state.last_error(),
        watchers: entry.tap.watchers(),
    }
}

// ----------------------------------------------------------------- requests

#[derive(Deserialize)]
pub struct NewPortReq {
    pub device: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_protocol")]
    pub protocol: ProtocolArg,
    #[serde(flatten)]
    pub serial: SerialArgs,
    #[serde(default)]
    pub tcp_port: Option<u16>,
    #[serde(default)]
    pub expose: bool,
    /// Start it immediately rather than leaving it paired but idle.
    #[serde(default)]
    pub start: bool,
}

fn default_protocol() -> ProtocolArg {
    ProtocolArg::Raw
}

/// Every field optional: absent means "leave this alone".
#[derive(Deserialize, Default)]
pub struct PatchReq {
    pub label: Option<String>,
    pub tcp_port: Option<u16>,
    pub protocol: Option<ProtocolArg>,
    pub baud: Option<u32>,
    pub data_bits: Option<DataBitsArg>,
    pub parity: Option<ParityArg>,
    pub stop_bits: Option<StopBitsArg>,
    pub flow_control: Option<FlowControlArg>,
    pub expose: Option<bool>,
    pub autostart: Option<bool>,
}

#[derive(Deserialize)]
pub struct SendReq {
    pub data: String,
    /// `text` or `hex`.
    #[serde(default = "default_encoding")]
    pub encoding: String,
    /// `cr`, `lf` or `crlf`. Many devices need a line ending and typing one into
    /// a text box is not possible.
    #[serde(default)]
    pub append: Option<String>,
}

fn default_encoding() -> String {
    "text".to_owned()
}

// ----------------------------------------------------------------- handlers

pub fn devices(registry: &Registry) -> ApiResult {
    let paired: Vec<String> = registry
        .entries()
        .iter()
        .map(|e| e.device.clone())
        .collect();

    let devices: Vec<DeviceDto> = list::enumerate(false)
        .map_err(ApiError::from)?
        .into_iter()
        .map(|info| DeviceDto {
            paired: paired.contains(&info.port_name),
            description: list::describe(&info.port_type),
            device: info.port_name,
        })
        .collect();

    Ok(json!({ "devices": devices }))
}

pub fn ports(registry: &Registry) -> ApiResult {
    let ports: Vec<PortDto> = registry.entries().iter().map(port_dto).collect();
    let allow = registry.allowlist();

    Ok(json!({
        "ports": ports,
        // Sent with every state update so the page can say out loud how open it
        // is — an unguarded dashboard should not look like a guarded one.
        "access": {
            "require_token": registry.require_token(),
            "allow": allow.rules().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "summary": allow.describe(),
        },
    }))
}

pub fn create(registry: &Registry, body: &str) -> ApiResult {
    let req: NewPortReq = parse(body)?;
    if req.device.trim().is_empty() {
        return Err(ApiError::bad_request("a device name is required"));
    }

    let entry = registry.add(
        &req.device,
        &req.label,
        req.protocol,
        req.serial,
        req.tcp_port,
        req.expose,
    )?;

    if req.start {
        registry.start(&entry.id)?;
    }

    Ok(json!({ "port": port_dto(&entry) }))
}

pub fn update(registry: &Registry, id: &str, body: &str) -> ApiResult {
    let req: PatchReq = parse(body)?;
    let entry = registry
        .entry(id)
        .ok_or_else(|| ApiError::not_found(format!("no port with id {id}")))?;

    // Line settings arrive field by field, so start from what the port has and
    // overwrite only what was sent.
    let mut serial = entry.config().serial;
    let mut touched = false;
    if let Some(v) = req.baud {
        serial.baud = v;
        touched = true;
    }
    if let Some(v) = req.data_bits {
        serial.data_bits = v;
        touched = true;
    }
    if let Some(v) = req.parity {
        serial.parity = v;
        touched = true;
    }
    if let Some(v) = req.stop_bits {
        serial.stop_bits = v;
        touched = true;
    }
    if let Some(v) = req.flow_control {
        serial.flow_control = v;
        touched = true;
    }

    let patch = PortPatch {
        label: req.label,
        tcp_port: req.tcp_port,
        protocol: req.protocol,
        serial: touched.then_some(serial),
        expose: req.expose,
        autostart: req.autostart,
    };

    let entry = registry.update(id, patch)?;
    Ok(json!({ "port": port_dto(&entry) }))
}

pub fn delete(registry: &Registry, id: &str) -> ApiResult {
    ensure_known(registry, id)?;
    registry.remove(id)?;
    Ok(json!({ "ok": true }))
}

pub fn start(registry: &Registry, id: &str) -> ApiResult {
    let entry = ensure_known(registry, id)?;
    registry.start(id)?;
    Ok(json!({ "port": port_dto(&entry) }))
}

pub fn stop(registry: &Registry, id: &str) -> ApiResult {
    let entry = ensure_known(registry, id)?;
    registry.stop(id)?;
    Ok(json!({ "port": port_dto(&entry) }))
}

pub fn send(registry: &Registry, id: &str, body: &str) -> ApiResult {
    let req: SendReq = parse(body)?;
    let entry = ensure_known(registry, id)?;

    let mut bytes = match req.encoding.as_str() {
        "text" => req.data.into_bytes(),
        "hex" => decode_hex(&req.data)?,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown encoding {other}, expected text or hex"
            )));
        }
    };

    match req.append.as_deref() {
        None | Some("") | Some("none") => {}
        Some("cr") => bytes.push(b'\r'),
        Some("lf") => bytes.push(b'\n'),
        Some("crlf") => bytes.extend_from_slice(b"\r\n"),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown line ending {other}, expected cr, lf or crlf"
            )));
        }
    }

    if bytes.is_empty() {
        return Err(ApiError::bad_request("nothing to send"));
    }

    let sent = bytes.len();
    entry.send(&bytes)?;
    Ok(json!({ "sent": sent }))
}

fn ensure_known(registry: &Registry, id: &str) -> Result<Arc<PortEntry>, ApiError> {
    registry
        .entry(id)
        .ok_or_else(|| ApiError::not_found(format!("no port with id {id}")))
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, ApiError> {
    let body = if body.trim().is_empty() { "{}" } else { body };
    serde_json::from_str(body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
}

pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Tolerant of the spacing people naturally type: `0A 0D`, `0a0d`, `0a:0d`.
fn decode_hex(text: &str) -> Result<Vec<u8>, ApiError> {
    let digits: Vec<char> = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != ',')
        .collect();

    if !digits.len().is_multiple_of(2) {
        return Err(ApiError::bad_request(
            "hex needs an even number of digits — every byte is two",
        ));
    }

    digits
        .chunks(2)
        .map(|pair| {
            let hi = pair[0]
                .to_digit(16)
                .ok_or_else(|| ApiError::bad_request(format!("{} is not a hex digit", pair[0])))?;
            let lo = pair[1]
                .to_digit(16)
                .ok_or_else(|| ApiError::bad_request(format!("{} is not a hex digit", pair[1])))?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(encode_hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(decode_hex("000aff").unwrap(), vec![0x00, 0x0a, 0xff]);
    }

    #[test]
    fn hex_ignores_the_separators_people_type() {
        assert_eq!(decode_hex("0A 0D").unwrap(), vec![0x0a, 0x0d]);
        assert_eq!(decode_hex("0a:0d").unwrap(), vec![0x0a, 0x0d]);
    }

    #[test]
    fn odd_hex_is_rejected_rather_than_padded() {
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
