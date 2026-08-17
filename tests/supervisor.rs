//! Starting, bridging and — the part `serve` could never do — stopping.
//!
//! Runs everywhere, including Windows, because the device is a
//! [`common::FakePort`] rather than a pseudo-terminal.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{FakeDevice, TempDir, fake_pair};
use serialport::SerialPort as _;

use serial_tcp::cli::{ProtocolArg, SerialArgs};
use serial_tcp::dashboard::config::{Config, PortConfig};
use serial_tcp::dashboard::registry::{DeviceOpener, PortEntry, Registry};

const PATIENCE: Duration = Duration::from_secs(3);

struct Harness {
    registry: Arc<Registry>,
    entry: Arc<PortEntry>,
    device: FakeDevice,
    _dir: TempDir,
}

/// A registry with one port wired to a fake device, asked for an ephemeral TCP
/// port so parallel tests never collide.
fn start(protocol: ProtocolArg) -> Harness {
    let dir = TempDir::new("supervisor");
    let (template, device) = fake_pair();

    let opener: DeviceOpener = Arc::new(move |_path, _settings| Ok(template.try_clone()?));

    let mut config = Config::new(4001, "test-token".to_owned());
    config.ports.push(PortConfig {
        id: "fake".to_owned(),
        device: "FAKE0".to_owned(),
        label: String::new(),
        tcp_port: 0,
        protocol,
        serial: SerialArgs::default(),
        expose: false,
        autostart: false,
    });

    let registry = Registry::new(&config, dir.join("serial-tcp.json"), opener);
    registry.start("fake").expect("the port should start");

    let entry = registry.entry("fake").expect("the entry should exist");
    assert!(entry.is_running());

    Harness {
        registry,
        entry,
        device,
        _dir: dir,
    }
}

fn connect(entry: &PortEntry) -> TcpStream {
    let addr = entry.bound().expect("a running port has an address");
    let stream = TcpStream::connect(addr).expect("connect to the port");
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    // The supervisor purges the serial buffers when a client arrives; let that
    // land before the test puts anything on the line.
    std::thread::sleep(Duration::from_millis(150));
    stream
}

#[test]
fn what_the_device_says_reaches_the_client() {
    let h = start(ProtocolArg::Raw);
    let mut client = connect(&h.entry);

    h.device
        .send(b"$GNGGA,000049.500,,,,,0,00,99.99,,M,,M,,*40\r\n");

    let mut buf = [0u8; 128];
    let n = client.read(&mut buf).expect("read from the client socket");
    assert!(
        buf[..n].starts_with(b"$GNGGA,"),
        "got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );

    h.registry.stop("fake").unwrap();
}

#[test]
fn what_the_client_writes_reaches_the_device() {
    let h = start(ProtocolArg::Raw);
    let mut client = connect(&h.entry);

    client.write_all(b"$PQTMSAVEPAR*5A\r\n").unwrap();
    client.flush().unwrap();

    let received = h.device.wait_for(17, PATIENCE);
    assert_eq!(received, b"$PQTMSAVEPAR*5A\r\n");

    h.registry.stop("fake").unwrap();
}

#[test]
fn traffic_is_counted_in_both_directions() {
    let h = start(ProtocolArg::Raw);
    let mut client = connect(&h.entry);

    assert_eq!(h.entry.tap.rx_bytes(), 0);
    assert_eq!(h.entry.tap.tx_bytes(), 0);

    h.device.send(b"hello");
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello");

    client.write_all(b"hi").unwrap();
    h.device.wait_for(2, PATIENCE);

    // The write side is counted as the bytes leave for the device, which the
    // pump does a moment after the socket read returns.
    let deadline = Instant::now() + PATIENCE;
    while h.entry.tap.tx_bytes() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(h.entry.tap.rx_bytes(), 5, "device -> network");
    assert_eq!(h.entry.tap.tx_bytes(), 2, "network -> device");

    h.registry.stop("fake").unwrap();
}

/// The whole point of the supervisor: `serve_on` can only ever stop when a
/// client leaves of its own accord.
#[test]
fn stopping_cuts_a_connected_client_loose_promptly() {
    let h = start(ProtocolArg::Raw);
    let mut client = connect(&h.entry);

    h.device.send(b"x");
    let mut buf = [0u8; 8];
    assert_eq!(client.read(&mut buf).unwrap(), 1);

    let began = Instant::now();
    h.registry.stop("fake").unwrap();
    let took = began.elapsed();

    assert!(
        took < Duration::from_secs(2),
        "stopping took {took:?}; it should not wait for the client to leave"
    );
    assert!(!h.entry.is_running());
    assert!(h.entry.bound().is_none(), "the listener should be released");

    // The socket is gone: reads return end-of-stream or an error, not data.
    match client.read(&mut buf) {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("still connected, read {n} bytes"),
    }
}

#[test]
fn a_stopped_port_frees_its_tcp_port() {
    let h = start(ProtocolArg::Raw);
    let addr = h.entry.bound().expect("running");
    h.registry.stop("fake").unwrap();

    std::net::TcpListener::bind(addr).expect("the address should be free again");
}

#[test]
fn stopping_twice_is_harmless() {
    let h = start(ProtocolArg::Raw);
    h.registry.stop("fake").unwrap();
    h.registry.stop("fake").unwrap();
    assert!(!h.entry.is_running());
}

/// The send box has to work between clients, which is why the write handle
/// belongs to the port rather than to a session.
#[test]
fn the_dashboard_can_write_with_no_client_connected() {
    let h = start(ProtocolArg::Raw);

    h.entry.send(b"$PQTMVERNO*58\r\n").expect("send");

    let received = h.device.wait_for(15, PATIENCE);
    assert_eq!(received, b"$PQTMVERNO*58\r\n");
    assert_eq!(h.entry.tap.tx_bytes(), 15, "injected bytes are counted too");

    h.registry.stop("fake").unwrap();
}

#[test]
fn a_stopped_port_refuses_to_send() {
    let h = start(ProtocolArg::Raw);
    h.registry.stop("fake").unwrap();

    let err = h.entry.send(b"nope").expect_err("should refuse");
    assert!(
        err.to_string().contains("not running"),
        "unhelpful error: {err}"
    );
}

#[test]
fn a_second_client_is_served_after_the_first_leaves() {
    let h = start(ProtocolArg::Raw);

    {
        let mut first = connect(&h.entry);
        h.device.send(b"one");
        let mut buf = [0u8; 8];
        assert_eq!(first.read(&mut buf).unwrap(), 3);
    } // dropped: the first client disconnects

    let mut second = connect(&h.entry);
    h.device.send(b"two");
    let mut buf = [0u8; 8];
    let n = second
        .read(&mut buf)
        .expect("the second client should be served");
    assert_eq!(&buf[..n], b"two");

    h.registry.stop("fake").unwrap();
}

/// Watching from a browser must not change what the wire does.
#[test]
fn a_watcher_sees_the_traffic_without_intercepting_it() {
    let h = start(ProtocolArg::Raw);
    let mut client = connect(&h.entry);

    let subscription = h.entry.tap.subscribe();
    assert_eq!(h.entry.tap.watchers(), 1);

    h.device.send(b"watched");
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"watched", "the client still gets everything");

    let frame = subscription
        .recv_timeout(PATIENCE)
        .expect("the watcher should see it too");
    assert_eq!(frame.data, b"watched");
    assert_eq!(frame.dir.as_str(), "rx");

    drop(subscription);
    assert_eq!(h.entry.tap.watchers(), 0, "dropping unsubscribes");

    h.registry.stop("fake").unwrap();
}

/// The monitor is mainly wanted for watching a device talk, which must not
/// require standing a TCP client up first.
#[test]
fn a_watcher_sees_the_device_with_no_client_connected() {
    let h = start(ProtocolArg::Raw);
    let subscription = h.entry.tap.subscribe();

    h.device.send(b"$GNRMC,unattended\r\n");

    let frame = subscription
        .recv_timeout(PATIENCE)
        .expect("the monitor should show traffic with nobody bridged");
    assert!(
        frame.data.starts_with(b"$GNRMC,"),
        "got {:?}",
        String::from_utf8_lossy(&frame.data)
    );

    h.registry.stop("fake").unwrap();
}

/// Draining for a watcher must not become a way to lose a client's first bytes.
#[test]
fn nothing_is_read_from_the_line_while_nobody_is_watching() {
    let h = start(ProtocolArg::Raw);

    h.device.send(b"said-before-anyone-listened");
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        h.entry.tap.rx_bytes(),
        0,
        "with no watcher and no client, the line should be left alone"
    );

    h.registry.stop("fake").unwrap();
}

#[test]
fn rfc2217_ports_bridge_data_too() {
    let h = start(ProtocolArg::Rfc2217);
    let mut client = connect(&h.entry);

    h.device.send(b"plain");

    // The server opens with Telnet negotiation, so the payload arrives after
    // some control bytes; read until the text shows up.
    let deadline = Instant::now() + PATIENCE;
    let mut seen = Vec::new();
    while Instant::now() < deadline && !contains(&seen, b"plain") {
        let mut buf = [0u8; 256];
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    assert!(
        contains(&seen, b"plain"),
        "payload never arrived; got {seen:?}"
    );

    h.registry.stop("fake").unwrap();
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
