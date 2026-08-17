//! RFC 2217 — Telnet Com Port Control Option.
//!
//! Raw mode moves bytes and nothing else. RFC 2217 wraps that byte stream in
//! Telnet framing so the two ends can also agree on baud rate, character
//! format, flow control and the modem control lines — which is what makes a
//! remote port behave like a local one.
//!
//! It is also what everyone else already speaks: `ser2net`, commercial device
//! servers, and pyserial via `rfc2217://host:port`.
//!
//! <https://www.rfc-editor.org/rfc/rfc2217.html>

pub mod codec;
pub mod comport;

// Telnet framing (RFC 854).
pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;

// Telnet options we take part in. Anything else is refused.
pub const OPT_BINARY: u8 = 0;
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_COM_PORT: u8 = 44;

// COM-PORT-OPTION commands, as sent by the client.
pub const SIGNATURE: u8 = 0;
pub const SET_BAUDRATE: u8 = 1;
pub const SET_DATASIZE: u8 = 2;
pub const SET_PARITY: u8 = 3;
pub const SET_STOPSIZE: u8 = 4;
pub const SET_CONTROL: u8 = 5;
pub const NOTIFY_LINESTATE: u8 = 6;
pub const NOTIFY_MODEMSTATE: u8 = 7;
pub const FLOWCONTROL_SUSPEND: u8 = 8;
pub const FLOWCONTROL_RESUME: u8 = 9;
pub const SET_LINESTATE_MASK: u8 = 10;
pub const SET_MODEMSTATE_MASK: u8 = 11;
pub const PURGE_DATA: u8 = 12;

/// The server answers with the client's command code plus this offset, so
/// `SET_BAUDRATE` (1) is answered with 101.
pub const SERVER_OFFSET: u8 = 100;

// SET-CONTROL values.
pub const CONTROL_REQ_FLOW: u8 = 0;
pub const CONTROL_FLOW_NONE: u8 = 1;
pub const CONTROL_FLOW_XONXOFF: u8 = 2;
pub const CONTROL_FLOW_HARDWARE: u8 = 3;
pub const CONTROL_REQ_BREAK: u8 = 4;
pub const CONTROL_BREAK_ON: u8 = 5;
pub const CONTROL_BREAK_OFF: u8 = 6;
pub const CONTROL_REQ_DTR: u8 = 7;
pub const CONTROL_DTR_ON: u8 = 8;
pub const CONTROL_DTR_OFF: u8 = 9;
pub const CONTROL_REQ_RTS: u8 = 10;
pub const CONTROL_RTS_ON: u8 = 11;
pub const CONTROL_RTS_OFF: u8 = 12;

// Modem state bits, laid out like a 16550 UART's modem status register.
pub const MODEM_DELTA_CTS: u8 = 0x01;
pub const MODEM_DELTA_DSR: u8 = 0x02;
pub const MODEM_TRAILING_RI: u8 = 0x04;
pub const MODEM_DELTA_CD: u8 = 0x08;
pub const MODEM_CTS: u8 = 0x10;
pub const MODEM_DSR: u8 = 0x20;
pub const MODEM_RI: u8 = 0x40;
pub const MODEM_CD: u8 = 0x80;

/// Identifies this implementation in a SIGNATURE exchange.
pub const SIGNATURE_TEXT: &[u8] = b"serial-tcp";
