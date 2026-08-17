//! RFC 2217 end to end, against our own server, with no hardware.
//!
//! The interoperability that matters most is with other implementations —
//! pyserial's `rfc2217://` client exercises this same server — but that needs
//! Python installed. These tests cover the same ground in-process so a plain
//! `cargo test` still catches a regression in the framing or the command
//! replies.

#![cfg(unix)]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serialport::SerialPort;

use serial_tcp::cli::{ProtocolArg, SerialArgs};
use serial_tcp::endpoint::IO_TIMEOUT;
use serial_tcp::rfc2217::codec::{
    Decoder, EscapingWriter, Handler, SharedWriter, TelnetReader, share, subnegotiation, write_raw,
};
use serial_tcp::rfc2217::{SERVER_OFFSET, SET_BAUDRATE};
use serial_tcp::serial;
use serial_tcp::server::{Options, serve_on};

const PATIENCE: Duration = Duration::from_secs(10);

/// Records the com port replies the server sends back.
type Replies = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

struct TestClient {
    out: SharedWriter,
    replies: Replies,
}

impl Handler for TestClient {
    fn com_port(&mut self, command: u8, payload: &[u8]) -> std::io::Result<()> {
        self.replies
            .lock()
            .unwrap()
            .push((command, payload.to_vec()));
        Ok(())
    }
    fn writer(&self) -> &SharedWriter {
        &self.out
    }
}

struct Harness {
    device: Box<dyn SerialPort>,
    reader: TelnetReader<TcpStream, TestClient>,
    writer: EscapingWriter,
    socket: SharedWriter,
    replies: Replies,
}

fn start() -> Harness {
    let pty = serial::open_pty().expect("create pseudo-terminal pair");
    let master = pty.master;
    let mut device = pty.keepalive;
    device.set_timeout(PATIENCE).expect("set device timeout");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("read bound address");

    let options = Options {
        protocol: ProtocolArg::Rfc2217,
        // A pseudo-terminal has no real line, which is exactly the case where
        // the server reports the requested settings rather than the port's.
        virtual_line: true,
        settings: SerialArgs::default(),
    };
    thread::spawn(move || {
        // Reported rather than unwrapped: a panic here would not fail the test,
        // it would just leave the client waiting for a server that is gone.
        if let Err(e) = serve_on(&listener, master.as_ref(), &options, Some(1)) {
            eprintln!("server stopped early: {e:?}");
        }
    });

    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set read timeout");
    let socket = share(Box::new(stream.try_clone().expect("clone stream")));

    let replies: Replies = Arc::new(Mutex::new(Vec::new()));
    let mut handler = TestClient {
        out: Arc::clone(&socket),
        replies: Arc::clone(&replies),
    };

    let mut decoder = Decoder::new();
    decoder.initiate(&mut handler).expect("negotiate");

    // The server purges the device's buffers when a client arrives, so anything
    // written before that lands is discarded by design. Let the session settle
    // before the test starts talking.
    thread::sleep(Duration::from_millis(150));

    Harness {
        device,
        reader: TelnetReader::new(stream, decoder, handler),
        writer: EscapingWriter::new(Arc::clone(&socket)),
        socket,
        replies,
    }
}

/// Read until `want` bytes have arrived, driving the decoder as we go.
///
/// Idle timeouts are expected: the reader only surfaces payload, and control
/// traffic is handled behind it.
fn read_payload(harness: &mut Harness, want: usize) -> Vec<u8> {
    let deadline = Instant::now() + PATIENCE;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];

    while got.len() < want && Instant::now() < deadline {
        match harness.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
            Err(e) => panic!("read failed: {e}"),
        }
    }
    got
}

#[test]
fn data_flows_both_ways_through_telnet_framing() {
    let mut h = start();

    h.device.write_all(b"device-side").expect("device write");
    h.device.flush().expect("device flush");
    assert_eq!(read_payload(&mut h, 11), b"device-side");

    h.writer.write_all(b"client-side").expect("client write");
    h.writer.flush().expect("client flush");

    let mut got = vec![0u8; 11];
    h.device.read_exact(&mut got).expect("device read");
    assert_eq!(got, b"client-side");
}

/// 0xFF is the byte Telnet framing steals. If escaping were wrong in either
/// direction it would be swallowed or misread as the start of a command.
#[test]
fn literal_ff_bytes_survive_in_both_directions() {
    let mut h = start();
    let tricky = [0xFFu8, 0x00, 0xFF, 0xFF, b'A', 0xFF];

    h.device.write_all(&tricky).expect("device write");
    h.device.flush().expect("device flush");
    assert_eq!(read_payload(&mut h, tricky.len()), tricky);

    h.writer.write_all(&tricky).expect("client write");
    h.writer.flush().expect("client flush");

    let mut got = vec![0u8; tricky.len()];
    h.device.read_exact(&mut got).expect("device read");
    assert_eq!(got, tricky);
}

#[test]
fn the_server_answers_a_baud_rate_request() {
    let mut h = start();

    write_raw(
        &h.socket,
        &subnegotiation(SET_BAUDRATE, &57_600u32.to_be_bytes()),
    )
    .expect("send baud request");

    let want = (
        SET_BAUDRATE + SERVER_OFFSET,
        57_600u32.to_be_bytes().to_vec(),
    );
    assert!(
        wait_for_reply(&mut h, &want),
        "expected a baud rate reply of 57600, got {:?}",
        h.replies.lock().unwrap()
    );
}

/// Pump the reader until `want` appears among the server's replies.
///
/// Control traffic is handled inside the reader rather than returned, so the
/// only way to make progress is to keep calling `read` and let the idle
/// timeouts fall through. Waiting for the reply rather than reading a byte of
/// payload and then checking matters: the reply and the device's data are
/// written to the socket by two different threads, so their order is not fixed.
fn wait_for_reply(harness: &mut Harness, want: &(u8, Vec<u8>)) -> bool {
    let deadline = Instant::now() + PATIENCE;
    let mut buf = [0u8; 256];

    while Instant::now() < deadline {
        if harness.replies.lock().unwrap().contains(want) {
            return true;
        }
        match harness.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
            Err(e) => panic!("read failed: {e}"),
        }
    }
    harness.replies.lock().unwrap().contains(want)
}
