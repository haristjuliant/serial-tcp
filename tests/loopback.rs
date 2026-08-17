//! End-to-end tests that need no serial hardware.
//!
//! A pseudo-terminal pair stands in for a real device: the server bridges the
//! master end, and the test drives the slave end as if it were the hardware on
//! the other side of the cable.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serialport::SerialPort;

use serial_tcp::serial;
use serial_tcp::server::{Options, serve_on};

/// Long enough that a healthy run never trips it, short enough that a hung run
/// fails instead of blocking the suite.
const PATIENCE: Duration = Duration::from_secs(10);

/// A running server plus the device end the test writes to and reads from.
struct Harness {
    addr: SocketAddr,
    device: Box<dyn SerialPort>,
    stopped: mpsc::Receiver<()>,
}

fn start(max_sessions: usize) -> Harness {
    let pty = serial::open_pty().expect("create pseudo-terminal pair");
    let master = pty.master;
    let mut device = pty.keepalive;
    device
        .set_timeout(PATIENCE)
        .expect("set device read timeout");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read bound address");

    let (tx, stopped) = mpsc::channel();
    thread::spawn(move || {
        serve_on(
            &listener,
            master.as_ref(),
            &Options::raw(),
            Some(max_sessions),
        )
        .expect("serve");
        let _ = tx.send(());
    });

    Harness {
        addr,
        device,
        stopped,
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect to server");
    stream
        .set_read_timeout(Some(PATIENCE))
        .expect("set client read timeout");
    // The server flushes stale device data when a client arrives; let that
    // happen before anyone starts writing.
    thread::sleep(Duration::from_millis(150));
    stream
}

/// Deterministic filler that would expose reordering or dropped chunks, which a
/// run of identical bytes would not.
fn payload(len: usize) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

fn read_exact_bytes(reader: &mut impl Read, len: usize) -> Vec<u8> {
    let mut got = vec![0u8; len];
    reader.read_exact(&mut got).expect("read expected bytes");
    got
}

#[test]
fn bytes_flow_from_device_to_client() {
    let mut h = start(1);
    let mut client = connect(h.addr);

    let message = b"hello from the device";
    h.device.write_all(message).expect("device write");
    h.device.flush().expect("device flush");

    assert_eq!(read_exact_bytes(&mut client, message.len()), message);
}

#[test]
fn bytes_flow_from_client_to_device() {
    let mut h = start(1);
    let mut client = connect(h.addr);

    let message = b"hello from the client";
    client.write_all(message).expect("client write");
    client.flush().expect("client flush");

    assert_eq!(read_exact_bytes(&mut h.device, message.len()), message);
}

#[test]
fn large_transfer_arrives_intact() {
    const SIZE: usize = 1 << 20; // 1 MiB

    let mut h = start(1);
    let mut client = connect(h.addr);

    let sent = payload(SIZE);
    let to_send = sent.clone();
    let writer = thread::spawn(move || {
        h.device.write_all(&to_send).expect("device write");
        h.device.flush().expect("device flush");
    });

    let received = read_exact_bytes(&mut client, SIZE);
    writer.join().expect("device writer thread");

    // Compare positions rather than the whole buffers so a failure says where
    // the streams diverged instead of dumping a megabyte.
    if let Some(i) = (0..SIZE).find(|&i| received[i] != sent[i]) {
        panic!(
            "streams diverge at byte {i}: got {:#x}, want {:#x}",
            received[i], sent[i]
        );
    }
}

/// Regression guard for the failure mode this design is built around: a thread
/// parked in a blocking read never noticing that the other direction died.
#[test]
fn session_ends_when_the_client_disconnects() {
    let h = start(1);
    let client = connect(h.addr);

    client
        .shutdown(std::net::Shutdown::Both)
        .expect("close client");
    drop(client);

    h.stopped
        .recv_timeout(PATIENCE)
        .expect("server should finish the session and return, not hang");
}
