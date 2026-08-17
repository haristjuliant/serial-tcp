//! Telnet framing: option negotiation, `IAC` escaping, subnegotiation parsing.
//!
//! Both ends of the link share this. The pieces are shaped as a plain `Read`
//! and a plain `Write`, so [`crate::bridge::bridge`] does not need to know that
//! anything more than raw bytes is going on.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use super::{DO, DONT, IAC, OPT_BINARY, OPT_COM_PORT, OPT_ECHO, OPT_SGA, SB, SE, WILL, WONT};

/// A socket shared between the data pump and the control channel.
///
/// Telnet commands and escaped data go down the same wire, so they must not
/// interleave halfway through a sequence.
pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub fn share(writer: Box<dyn Write + Send>) -> SharedWriter {
    Arc::new(Mutex::new(writer))
}

/// Write bytes through unchanged, holding the lock for the whole sequence.
pub fn write_raw(out: &SharedWriter, bytes: &[u8]) -> io::Result<()> {
    // A poisoned lock means another thread panicked mid-write. The stream may
    // have a truncated sequence on it, but refusing to write from here on would
    // just turn that into a hang.
    let mut guard = out.lock().unwrap_or_else(|e| e.into_inner());
    guard.write_all(bytes)?;
    guard.flush()
}

/// Reacts to the control traffic the decoder pulls out of the stream.
pub trait Handler: Send {
    /// A COM-PORT-OPTION subnegotiation arrived.
    fn com_port(&mut self, cmd: u8, payload: &[u8]) -> io::Result<()>;

    /// The socket, for sending replies.
    fn writer(&self) -> &SharedWriter;

    /// Both ends have agreed we may perform `option`. Fired at most once per
    /// option.
    fn option_agreed(&mut self, _option: u8) -> io::Result<()> {
        Ok(())
    }
}

/// Build a COM-PORT-OPTION subnegotiation, escaping `IAC` in the payload.
pub fn subnegotiation(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![IAC, SB, OPT_COM_PORT, command];
    for &byte in payload {
        if byte == IAC {
            out.push(IAC);
        }
        out.push(byte);
    }
    out.extend_from_slice(&[IAC, SE]);
    out
}

/// Options we are willing to perform ourselves.
///
/// ECHO is absent deliberately: this is a byte pipe and never echoes, so
/// claiming otherwise would be a lie the peer might act on.
fn we_perform(option: u8) -> bool {
    matches!(option, OPT_BINARY | OPT_SGA | OPT_COM_PORT)
}

/// Options we are willing to let the peer perform.
fn they_may_perform(option: u8) -> bool {
    matches!(option, OPT_BINARY | OPT_SGA | OPT_COM_PORT | OPT_ECHO)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    Iac,
    Will,
    Wont,
    Do,
    Dont,
    SubNeg,
    SubNegIac,
}

/// Pulls Telnet control traffic out of a byte stream, leaving the data behind.
pub struct Decoder {
    state: State,
    subneg: Vec<u8>,
    /// Options we have answered for, and whether we agreed to perform them.
    us: HashMap<u8, bool>,
    /// Options the peer has been answered about performing.
    them: HashMap<u8, bool>,
    agreed_fired: HashSet<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            state: State::Data,
            subneg: Vec::new(),
            us: HashMap::new(),
            them: HashMap::new(),
            agreed_fired: HashSet::new(),
        }
    }

    /// Offer and request the options we care about.
    ///
    /// The offers are recorded before being sent so that the peer's agreement
    /// does not look like a fresh request and bounce back and forth forever.
    pub fn initiate(&mut self, handler: &mut dyn Handler) -> io::Result<()> {
        let mut message = Vec::new();
        for option in [OPT_BINARY, OPT_SGA, OPT_COM_PORT] {
            self.us.insert(option, true);
            self.them.insert(option, true);
            message.extend_from_slice(&[IAC, WILL, option, IAC, DO, option]);
        }
        write_raw(handler.writer(), &message)
    }

    /// Consume `input`, appending payload bytes to `data`.
    pub fn feed(
        &mut self,
        input: &[u8],
        data: &mut Vec<u8>,
        handler: &mut dyn Handler,
    ) -> io::Result<()> {
        for &byte in input {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        self.state = State::Iac;
                    } else {
                        data.push(byte);
                    }
                }
                State::Iac => match byte {
                    IAC => {
                        // Doubled IAC is a literal 0xFF in the data.
                        data.push(IAC);
                        self.state = State::Data;
                    }
                    WILL => self.state = State::Will,
                    WONT => self.state = State::Wont,
                    DO => self.state = State::Do,
                    DONT => self.state = State::Dont,
                    SB => {
                        self.subneg.clear();
                        self.state = State::SubNeg;
                    }
                    // Two-byte commands (NOP, AYT, ...) carry nothing we need.
                    _ => self.state = State::Data,
                },
                State::Will => {
                    self.on_will(byte, handler)?;
                    self.state = State::Data;
                }
                State::Wont => {
                    self.on_wont(byte, handler)?;
                    self.state = State::Data;
                }
                State::Do => {
                    self.on_do(byte, handler)?;
                    self.state = State::Data;
                }
                State::Dont => {
                    self.on_dont(byte, handler)?;
                    self.state = State::Data;
                }
                State::SubNeg => {
                    if byte == IAC {
                        self.state = State::SubNegIac;
                    } else {
                        self.subneg.push(byte);
                    }
                }
                State::SubNegIac => match byte {
                    IAC => {
                        self.subneg.push(IAC);
                        self.state = State::SubNeg;
                    }
                    SE => {
                        self.dispatch_subnegotiation(handler)?;
                        self.state = State::Data;
                    }
                    // Malformed; the peer abandoned the subnegotiation.
                    _ => self.state = State::SubNeg,
                },
            }
        }
        Ok(())
    }

    fn dispatch_subnegotiation(&mut self, handler: &mut dyn Handler) -> io::Result<()> {
        let sub = std::mem::take(&mut self.subneg);
        match sub.split_first() {
            Some((&OPT_COM_PORT, rest)) => match rest.split_first() {
                Some((&command, payload)) => handler.com_port(command, payload),
                None => Ok(()),
            },
            Some((&other, _)) => {
                log::debug!("ignoring subnegotiation for option {other}");
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// The peer offers to perform `option`.
    fn on_will(&mut self, option: u8, handler: &mut dyn Handler) -> io::Result<()> {
        let accept = they_may_perform(option);
        self.answer(option, accept, Side::Them, handler)
    }

    /// The peer declines to perform `option`.
    fn on_wont(&mut self, option: u8, handler: &mut dyn Handler) -> io::Result<()> {
        self.answer(option, false, Side::Them, handler)
    }

    /// The peer asks us to perform `option`.
    fn on_do(&mut self, option: u8, handler: &mut dyn Handler) -> io::Result<()> {
        let accept = we_perform(option);
        self.answer(option, accept, Side::Us, handler)?;
        if accept && self.agreed_fired.insert(option) {
            handler.option_agreed(option)?;
        }
        Ok(())
    }

    /// The peer asks us not to perform `option`.
    fn on_dont(&mut self, option: u8, handler: &mut dyn Handler) -> io::Result<()> {
        self.answer(option, false, Side::Us, handler)
    }

    /// Record an option's state and reply, but only when the state changed.
    ///
    /// Replying unconditionally is how Telnet negotiation loops start.
    fn answer(
        &mut self,
        option: u8,
        agree: bool,
        side: Side,
        handler: &mut dyn Handler,
    ) -> io::Result<()> {
        let table = match side {
            Side::Us => &mut self.us,
            Side::Them => &mut self.them,
        };
        if table.get(&option) == Some(&agree) {
            return Ok(());
        }
        table.insert(option, agree);

        let verb = match (side, agree) {
            (Side::Us, true) => WILL,
            (Side::Us, false) => WONT,
            (Side::Them, true) => DO,
            (Side::Them, false) => DONT,
        };
        write_raw(handler.writer(), &[IAC, verb, option])
    }
}

#[derive(Clone, Copy)]
enum Side {
    Us,
    Them,
}

/// Reader that yields only the payload of a Telnet stream.
pub struct TelnetReader<R: Read, H: Handler> {
    inner: R,
    decoder: Decoder,
    handler: H,
    raw: [u8; 4096],
    data: Vec<u8>,
    taken: usize,
}

impl<R: Read, H: Handler> TelnetReader<R, H> {
    pub fn new(inner: R, decoder: Decoder, handler: H) -> Self {
        Self {
            inner,
            decoder,
            handler,
            raw: [0u8; 4096],
            data: Vec::new(),
            taken: 0,
        }
    }
}

impl<R: Read, H: Handler> Read for TelnetReader<R, H> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.taken < self.data.len() {
                let n = (self.data.len() - self.taken).min(out.len());
                out[..n].copy_from_slice(&self.data[self.taken..self.taken + n]);
                self.taken += n;
                return Ok(n);
            }

            self.data.clear();
            self.taken = 0;

            // Timeouts propagate to the caller, which treats them as an idle
            // line. A genuine zero-length read is end of stream.
            let n = self.inner.read(&mut self.raw)?;
            if n == 0 {
                return Ok(0);
            }

            self.decoder
                .feed(&self.raw[..n], &mut self.data, &mut self.handler)?;

            // A chunk of pure control traffic yields no payload. Returning 0
            // here would be read as end of stream, so go round again.
        }
    }
}

/// Writer that escapes `IAC` so data can never be mistaken for a command.
pub struct EscapingWriter {
    out: SharedWriter,
    scratch: Vec<u8>,
}

impl EscapingWriter {
    pub fn new(out: SharedWriter) -> Self {
        Self {
            out,
            scratch: Vec::with_capacity(4096),
        }
    }
}

impl Write for EscapingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.scratch.clear();
        self.scratch.reserve(buf.len());
        for &byte in buf {
            if byte == IAC {
                self.scratch.push(IAC);
            }
            self.scratch.push(byte);
        }
        write_raw(&self.out, &self.scratch)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        guard.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfc2217::{SET_BAUDRATE, SIGNATURE};

    /// A `Write` that keeps everything, so tests can assert on what went out.
    #[derive(Clone)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct TestHandler {
        out: SharedWriter,
        sent: Arc<Mutex<Vec<u8>>>,
        commands: Vec<(u8, Vec<u8>)>,
        agreed: Vec<u8>,
    }

    impl TestHandler {
        fn new() -> Self {
            let sent = Arc::new(Mutex::new(Vec::new()));
            Self {
                out: share(Box::new(Recorder(Arc::clone(&sent)))),
                sent,
                commands: Vec::new(),
                agreed: Vec::new(),
            }
        }

        fn outgoing(&self) -> Vec<u8> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Handler for TestHandler {
        fn com_port(&mut self, cmd: u8, payload: &[u8]) -> io::Result<()> {
            self.commands.push((cmd, payload.to_vec()));
            Ok(())
        }
        fn writer(&self) -> &SharedWriter {
            &self.out
        }
        fn option_agreed(&mut self, option: u8) -> io::Result<()> {
            self.agreed.push(option);
            Ok(())
        }
    }

    /// Feed bytes through a fresh decoder and return the payload plus handler.
    fn decode(input: &[u8]) -> (Vec<u8>, TestHandler) {
        let mut handler = TestHandler::new();
        let mut decoder = Decoder::new();
        let mut data = Vec::new();
        decoder.feed(input, &mut data, &mut handler).unwrap();
        (data, handler)
    }

    #[test]
    fn doubled_iac_decodes_to_one_literal_byte() {
        let (data, _) = decode(&[b'a', IAC, IAC, b'b']);
        assert_eq!(data, vec![b'a', 0xFF, b'b']);
    }

    #[test]
    fn escaping_writer_doubles_iac() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut writer = EscapingWriter::new(share(Box::new(Recorder(Arc::clone(&sent)))));
        writer.write_all(&[0xFF, b'A', 0xFF]).unwrap();
        assert_eq!(*sent.lock().unwrap(), vec![0xFF, 0xFF, b'A', 0xFF, 0xFF]);
    }

    #[test]
    fn a_round_trip_through_both_halves_is_lossless() {
        let payload: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();

        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut writer = EscapingWriter::new(share(Box::new(Recorder(Arc::clone(&sent)))));
        writer.write_all(&payload).unwrap();

        let encoded = sent.lock().unwrap().clone();
        let (decoded, _) = decode(&encoded);
        assert_eq!(decoded, payload);
    }

    #[test]
    fn we_accept_options_we_support_and_refuse_the_rest() {
        let (_, handler) = decode(&[IAC, DO, OPT_COM_PORT, IAC, DO, 99]);
        assert_eq!(
            handler.outgoing(),
            vec![IAC, WILL, OPT_COM_PORT, IAC, WONT, 99]
        );
        assert_eq!(handler.agreed, vec![OPT_COM_PORT]);
    }

    /// We never claim to echo, because we do not.
    #[test]
    fn we_refuse_to_perform_echo_but_let_the_peer_echo() {
        let (_, handler) = decode(&[IAC, DO, OPT_ECHO, IAC, WILL, OPT_ECHO]);
        assert_eq!(
            handler.outgoing(),
            vec![IAC, WONT, OPT_ECHO, IAC, DO, OPT_ECHO]
        );
    }

    /// Answering an unchanged state is how negotiation loops start.
    #[test]
    fn a_repeated_request_is_not_answered_twice() {
        let (_, handler) = decode(&[IAC, DO, OPT_COM_PORT, IAC, DO, OPT_COM_PORT]);
        assert_eq!(handler.outgoing(), vec![IAC, WILL, OPT_COM_PORT]);
        assert_eq!(handler.agreed, vec![OPT_COM_PORT], "should fire once");
    }

    #[test]
    fn subnegotiation_payload_reaches_the_handler() {
        // 115200 == 0x0001C200
        let (data, handler) = decode(&[
            IAC,
            SB,
            OPT_COM_PORT,
            SET_BAUDRATE,
            0x00,
            0x01,
            0xC2,
            0x00,
            IAC,
            SE,
        ]);
        assert!(data.is_empty(), "control traffic is not payload");
        assert_eq!(handler.commands, vec![(SET_BAUDRATE, vec![0, 1, 0xC2, 0])]);
    }

    #[test]
    fn iac_inside_a_subnegotiation_is_unescaped() {
        let (_, handler) = decode(&[
            IAC,
            SB,
            OPT_COM_PORT,
            SIGNATURE,
            b'a',
            IAC,
            IAC,
            b'b',
            IAC,
            SE,
        ]);
        assert_eq!(handler.commands, vec![(SIGNATURE, vec![b'a', 0xFF, b'b'])]);
    }

    #[test]
    fn subnegotiations_are_built_with_escaping() {
        assert_eq!(
            subnegotiation(SIGNATURE, &[0xFF, b'x']),
            vec![IAC, SB, OPT_COM_PORT, SIGNATURE, 0xFF, 0xFF, b'x', IAC, SE]
        );
    }

    /// The state machine has to survive a command being split across reads,
    /// which is entirely normal on a socket.
    #[test]
    fn commands_split_across_chunks_still_parse() {
        let mut handler = TestHandler::new();
        let mut decoder = Decoder::new();
        let mut data = Vec::new();

        let whole = [
            b'x',
            IAC,
            SB,
            OPT_COM_PORT,
            SET_BAUDRATE,
            0x00,
            0x01,
            0xC2,
            0x00,
            IAC,
            SE,
            b'y',
        ];
        for byte in whole {
            decoder.feed(&[byte], &mut data, &mut handler).unwrap();
        }

        assert_eq!(data, vec![b'x', b'y']);
        assert_eq!(handler.commands, vec![(SET_BAUDRATE, vec![0, 1, 0xC2, 0])]);
    }

    /// A read that contains only control traffic must not look like EOF.
    #[test]
    fn control_only_input_does_not_end_the_stream() {
        let stream = std::io::Cursor::new(vec![
            IAC,
            DO,
            OPT_COM_PORT,
            IAC,
            WILL,
            OPT_BINARY,
            b'h',
            b'i',
        ]);
        let mut reader = TelnetReader::new(stream, Decoder::new(), TestHandler::new());

        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hi");
    }
}
