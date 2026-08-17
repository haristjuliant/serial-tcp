//! The COM-PORT-OPTION commands themselves: applying them to a real port on
//! the server side, and asking for them on the client side.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

use super::codec::{Handler, SharedWriter, subnegotiation, write_raw};
use super::*;
use crate::cli::SerialArgs;

/// How often the modem control lines are sampled.
///
/// These are handshake signals, not data — a change matters within human
/// reaction time, and polling harder would burn a syscall per line for nothing.
const MODEM_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Server side: applies the client's requests to the port and reports back.
pub struct ServerHandler {
    /// The single handle through which settings are changed.
    ///
    /// `serialport` caches settings per handle, so mutating them through more
    /// than one would let the copies disagree. The pumps hold their own clones
    /// and only ever move bytes.
    control: Box<dyn SerialPort>,
    out: SharedWriter,
    modem_mask: Arc<AtomicU8>,
    dtr: bool,
    rts: bool,
    break_on: bool,
    /// Bookkeeping used when the device has no real serial line.
    ///
    /// A pseudo-terminal has no baud rate, so `tcsetattr` rejects the change
    /// and the port keeps reporting whatever it was born with. Echoing that
    /// back would be reporting a fiction — and a conforming client such as
    /// pyserial treats the mismatch as a rejection and refuses to open. With
    /// nothing real to describe, remembering what was asked for is both honest
    /// and usable. Real ports never take this path: there we always report what
    /// the hardware actually did.
    shadow: Option<Line>,
}

/// The line settings, as last requested.
#[derive(Clone, Copy)]
struct Line {
    baud: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow: FlowControl,
}

impl ServerHandler {
    pub fn new(
        control: Box<dyn SerialPort>,
        out: SharedWriter,
        modem_mask: Arc<AtomicU8>,
        virtual_line: bool,
        defaults: &SerialArgs,
    ) -> Self {
        Self {
            control,
            out,
            modem_mask,
            dtr: false,
            rts: false,
            break_on: false,
            shadow: virtual_line.then(|| Line {
                baud: defaults.baud,
                data_bits: defaults.data_bits.into(),
                parity: defaults.parity.into(),
                stop_bits: defaults.stop_bits.into(),
                flow: defaults.flow_control.into(),
            }),
        }
    }

    fn reply(&self, command: u8, payload: &[u8]) -> io::Result<()> {
        write_raw(&self.out, &subnegotiation(command + SERVER_OFFSET, payload))
    }

    fn set_baud_rate(&mut self, payload: &[u8]) -> io::Result<()> {
        let requested = payload
            .first_chunk::<4>()
            .map_or(0, |b| u32::from_be_bytes(*b));

        // Zero means "tell me the current value" rather than "set zero".
        let current = if let Some(line) = &mut self.shadow {
            if requested != 0 {
                line.baud = requested;
            }
            line.baud
        } else {
            if requested != 0
                && let Err(e) = self.control.set_baud_rate(requested)
            {
                log::warn!("client asked for {requested} baud, which the port refused: {e}");
            }
            self.control.baud_rate().unwrap_or(requested)
        };

        log::debug!("baud rate is now {current}");
        self.reply(SET_BAUDRATE, &current.to_be_bytes())
    }

    fn set_data_size(&mut self, payload: &[u8]) -> io::Result<()> {
        let requested = payload.first().copied().and_then(data_bits_from_wire);

        let current = if let Some(line) = &mut self.shadow {
            line.data_bits = requested.unwrap_or(line.data_bits);
            line.data_bits
        } else {
            if let Some(bits) = requested
                && let Err(e) = self.control.set_data_bits(bits)
            {
                log::warn!("could not set data bits: {e}");
            }
            match self.control.data_bits() {
                Ok(bits) => bits,
                Err(_) => return self.reply(SET_DATASIZE, &[0]),
            }
        };

        self.reply(SET_DATASIZE, &[data_bits_to_wire(current)])
    }

    fn set_parity(&mut self, payload: &[u8]) -> io::Result<()> {
        let requested = payload.first().copied().and_then(parity_from_wire);

        let current = if let Some(line) = &mut self.shadow {
            line.parity = requested.unwrap_or(line.parity);
            line.parity
        } else {
            if let Some(parity) = requested
                && let Err(e) = self.control.set_parity(parity)
            {
                log::warn!("could not set parity: {e}");
            }
            match self.control.parity() {
                Ok(parity) => parity,
                Err(_) => return self.reply(SET_PARITY, &[0]),
            }
        };

        self.reply(SET_PARITY, &[parity_to_wire(current)])
    }

    fn set_stop_size(&mut self, payload: &[u8]) -> io::Result<()> {
        let requested = payload.first().copied().and_then(stop_bits_from_wire);

        let current = if let Some(line) = &mut self.shadow {
            line.stop_bits = requested.unwrap_or(line.stop_bits);
            line.stop_bits
        } else {
            if let Some(stop) = requested
                && let Err(e) = self.control.set_stop_bits(stop)
            {
                log::warn!("could not set stop bits: {e}");
            }
            match self.control.stop_bits() {
                Ok(stop) => stop,
                Err(_) => return self.reply(SET_STOPSIZE, &[0]),
            }
        };

        self.reply(SET_STOPSIZE, &[stop_bits_to_wire(current)])
    }

    fn set_control(&mut self, payload: &[u8]) -> io::Result<()> {
        let Some(&request) = payload.first() else {
            return Ok(());
        };

        let answer = match request {
            CONTROL_REQ_FLOW => match &self.shadow {
                Some(line) => flow_to_wire(line.flow),
                None => self.control.flow_control().map_or(0, flow_to_wire),
            },

            CONTROL_FLOW_NONE | CONTROL_FLOW_XONXOFF | CONTROL_FLOW_HARDWARE => {
                let requested = flow_from_wire(request);
                if let Some(line) = &mut self.shadow {
                    line.flow = requested.unwrap_or(line.flow);
                    flow_to_wire(line.flow)
                } else {
                    if let Some(flow) = requested
                        && let Err(e) = self.control.set_flow_control(flow)
                    {
                        log::warn!("could not set flow control: {e}");
                    }
                    self.control.flow_control().map_or(request, flow_to_wire)
                }
            }

            CONTROL_BREAK_ON => self.apply_break(true),
            CONTROL_BREAK_OFF => self.apply_break(false),
            // A query must not change anything — sending a break because the
            // client asked what the break state was would corrupt the line.
            CONTROL_REQ_BREAK => break_wire(self.break_on),

            CONTROL_DTR_ON => self.apply_dtr(true),
            CONTROL_DTR_OFF => self.apply_dtr(false),
            CONTROL_REQ_DTR => dtr_wire(self.dtr),

            CONTROL_RTS_ON => self.apply_rts(true),
            CONTROL_RTS_OFF => self.apply_rts(false),
            CONTROL_REQ_RTS => rts_wire(self.rts),

            // Inbound/outbound flow-control variants and anything newer: we do
            // not act on them, so echo rather than claim a state we did not set.
            other => other,
        };

        self.reply(SET_CONTROL, &[answer])
    }

    fn apply_break(&mut self, on: bool) -> u8 {
        if self.shadow.is_some() {
            self.break_on = on;
        } else {
            let result = if on {
                self.control.set_break()
            } else {
                self.control.clear_break()
            };
            match result {
                Ok(()) => self.break_on = on,
                Err(e) => log::warn!("could not change break state: {e}"),
            }
        }
        break_wire(self.break_on)
    }

    fn apply_dtr(&mut self, on: bool) -> u8 {
        if self.shadow.is_some() {
            self.dtr = on;
        } else {
            match self.control.write_data_terminal_ready(on) {
                Ok(()) => self.dtr = on,
                Err(e) => log::warn!("could not change DTR: {e}"),
            }
        }
        dtr_wire(self.dtr)
    }

    fn apply_rts(&mut self, on: bool) -> u8 {
        if self.shadow.is_some() {
            self.rts = on;
        } else {
            match self.control.write_request_to_send(on) {
                Ok(()) => self.rts = on,
                Err(e) => log::warn!("could not change RTS: {e}"),
            }
        }
        rts_wire(self.rts)
    }

    fn purge(&mut self, payload: &[u8]) -> io::Result<()> {
        let which = payload.first().copied().unwrap_or(3);
        let buffer = match which {
            1 => ClearBuffer::Input,
            2 => ClearBuffer::Output,
            _ => ClearBuffer::All,
        };
        if let Err(e) = self.control.clear(buffer) {
            log::warn!("could not purge buffers: {e}");
        }
        self.reply(PURGE_DATA, &[which])
    }
}

impl Handler for ServerHandler {
    fn writer(&self) -> &SharedWriter {
        &self.out
    }

    fn com_port(&mut self, command: u8, payload: &[u8]) -> io::Result<()> {
        match command {
            SIGNATURE => {
                if payload.is_empty() {
                    self.reply(SIGNATURE, SIGNATURE_TEXT)
                } else {
                    log::debug!("client signature: {}", String::from_utf8_lossy(payload));
                    Ok(())
                }
            }
            SET_BAUDRATE => self.set_baud_rate(payload),
            SET_DATASIZE => self.set_data_size(payload),
            SET_PARITY => self.set_parity(payload),
            SET_STOPSIZE => self.set_stop_size(payload),
            SET_CONTROL => self.set_control(payload),
            PURGE_DATA => self.purge(payload),
            SET_MODEMSTATE_MASK => {
                let mask = payload.first().copied().unwrap_or(0xFF);
                self.modem_mask.store(mask, Ordering::Relaxed);
                self.reply(SET_MODEMSTATE_MASK, &[mask])
            }
            SET_LINESTATE_MASK => {
                // Accepted so clients do not stall waiting for an answer. We do
                // not surface UART line status, so nothing is ever reported.
                let mask = payload.first().copied().unwrap_or(0);
                self.reply(SET_LINESTATE_MASK, &[mask])
            }
            FLOWCONTROL_SUSPEND | FLOWCONTROL_RESUME => self.reply(command, &[]),
            other => {
                log::debug!("ignoring unsupported com port command {other}");
                Ok(())
            }
        }
    }
}

/// Watch the modem control lines and tell the client when they change.
///
/// Without this the client can set DTR and RTS but never learn the state of
/// CTS, DSR, CD or RI — half of what makes these signals useful.
pub fn spawn_modem_notifier(
    mut port: Box<dyn SerialPort>,
    out: SharedWriter,
    mask: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut previous: Option<u8> = None;

        while !stop.load(Ordering::Relaxed) {
            let state = sample_modem_state(port.as_mut());
            let changed = previous.map_or(0xFF, |old| old ^ state);

            if changed & mask.load(Ordering::Relaxed) != 0 {
                let report = state | deltas_for(changed, previous, state);
                if write_raw(
                    &out,
                    &subnegotiation(NOTIFY_MODEMSTATE + SERVER_OFFSET, &[report]),
                )
                .is_err()
                {
                    // The session is over; the bridge will notice too.
                    break;
                }
            }
            previous = Some(state);
            thread::sleep(MODEM_POLL_INTERVAL);
        }
    })
}

fn sample_modem_state(port: &mut dyn SerialPort) -> u8 {
    let mut state = 0u8;
    if port.read_clear_to_send().unwrap_or(false) {
        state |= MODEM_CTS;
    }
    if port.read_data_set_ready().unwrap_or(false) {
        state |= MODEM_DSR;
    }
    if port.read_ring_indicator().unwrap_or(false) {
        state |= MODEM_RI;
    }
    if port.read_carrier_detect().unwrap_or(false) {
        state |= MODEM_CD;
    }
    state
}

/// Turn "these bits moved" into the delta flags the client expects.
///
/// RI is the odd one out: it reports only the trailing edge, so a ring starting
/// is not an event but a ring ending is.
fn deltas_for(changed: u8, previous: Option<u8>, state: u8) -> u8 {
    let mut deltas = 0;
    if changed & MODEM_CTS != 0 {
        deltas |= MODEM_DELTA_CTS;
    }
    if changed & MODEM_DSR != 0 {
        deltas |= MODEM_DELTA_DSR;
    }
    if changed & MODEM_CD != 0 {
        deltas |= MODEM_DELTA_CD;
    }
    if previous.is_some() && changed & MODEM_RI != 0 && state & MODEM_RI == 0 {
        deltas |= MODEM_TRAILING_RI;
    }
    deltas
}

/// Client side: asks the server for the line settings we were told to use.
pub struct ClientHandler {
    out: SharedWriter,
    settings: SerialArgs,
    requested: bool,
}

impl ClientHandler {
    pub fn new(out: SharedWriter, settings: SerialArgs) -> Self {
        Self {
            out,
            settings,
            requested: false,
        }
    }
}

impl Handler for ClientHandler {
    fn writer(&self) -> &SharedWriter {
        &self.out
    }

    fn com_port(&mut self, command: u8, payload: &[u8]) -> io::Result<()> {
        match command {
            c if c == SET_BAUDRATE + SERVER_OFFSET => {
                let baud = payload
                    .first_chunk::<4>()
                    .map_or(0, |b| u32::from_be_bytes(*b));
                log::info!("remote port is at {baud} baud");
            }
            c if c == NOTIFY_MODEMSTATE + SERVER_OFFSET => {
                if let Some(&state) = payload.first() {
                    log::debug!("modem state {state:#04x}");
                }
            }
            c if c == SIGNATURE + SERVER_OFFSET => {
                log::debug!("server signature: {}", String::from_utf8_lossy(payload));
            }
            other => log::debug!("server sent com port command {other}"),
        }
        Ok(())
    }

    fn option_agreed(&mut self, option: u8) -> io::Result<()> {
        if option != OPT_COM_PORT || self.requested {
            return Ok(());
        }
        self.requested = true;

        let s = &self.settings;
        let mut message = Vec::new();
        message.extend(subnegotiation(SET_BAUDRATE, &s.baud.to_be_bytes()));
        message.extend(subnegotiation(
            SET_DATASIZE,
            &[data_bits_to_wire(s.data_bits.into())],
        ));
        message.extend(subnegotiation(
            SET_PARITY,
            &[parity_to_wire(s.parity.into())],
        ));
        message.extend(subnegotiation(
            SET_STOPSIZE,
            &[stop_bits_to_wire(s.stop_bits.into())],
        ));
        message.extend(subnegotiation(
            SET_CONTROL,
            &[flow_to_wire(s.flow_control.into())],
        ));
        message.extend(subnegotiation(SET_MODEMSTATE_MASK, &[0xFF]));

        log::debug!("requesting {} baud on the remote port", s.baud);
        write_raw(&self.out, &message)
    }
}

fn dtr_wire(on: bool) -> u8 {
    if on { CONTROL_DTR_ON } else { CONTROL_DTR_OFF }
}

fn rts_wire(on: bool) -> u8 {
    if on { CONTROL_RTS_ON } else { CONTROL_RTS_OFF }
}

fn break_wire(on: bool) -> u8 {
    if on {
        CONTROL_BREAK_ON
    } else {
        CONTROL_BREAK_OFF
    }
}

fn data_bits_from_wire(value: u8) -> Option<DataBits> {
    match value {
        5 => Some(DataBits::Five),
        6 => Some(DataBits::Six),
        7 => Some(DataBits::Seven),
        8 => Some(DataBits::Eight),
        _ => None,
    }
}

fn data_bits_to_wire(bits: DataBits) -> u8 {
    match bits {
        DataBits::Five => 5,
        DataBits::Six => 6,
        DataBits::Seven => 7,
        DataBits::Eight => 8,
    }
}

fn parity_from_wire(value: u8) -> Option<Parity> {
    match value {
        1 => Some(Parity::None),
        2 => Some(Parity::Odd),
        3 => Some(Parity::Even),
        // 4 (mark) and 5 (space) exist in the RFC but not in the crate's API.
        _ => None,
    }
}

fn parity_to_wire(parity: Parity) -> u8 {
    match parity {
        Parity::None => 1,
        Parity::Odd => 2,
        Parity::Even => 3,
    }
}

fn stop_bits_from_wire(value: u8) -> Option<StopBits> {
    match value {
        1 => Some(StopBits::One),
        2 => Some(StopBits::Two),
        // 3 is 1.5 stop bits, which the crate cannot express.
        _ => None,
    }
}

fn stop_bits_to_wire(stop: StopBits) -> u8 {
    match stop {
        StopBits::One => 1,
        StopBits::Two => 2,
    }
}

fn flow_from_wire(value: u8) -> Option<FlowControl> {
    match value {
        CONTROL_FLOW_NONE => Some(FlowControl::None),
        CONTROL_FLOW_XONXOFF => Some(FlowControl::Software),
        CONTROL_FLOW_HARDWARE => Some(FlowControl::Hardware),
        _ => None,
    }
}

fn flow_to_wire(flow: FlowControl) -> u8 {
    match flow {
        FlowControl::None => CONTROL_FLOW_NONE,
        FlowControl::Software => CONTROL_FLOW_XONXOFF,
        FlowControl::Hardware => CONTROL_FLOW_HARDWARE,
    }
}
